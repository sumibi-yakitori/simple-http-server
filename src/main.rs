#[macro_use]
extern crate clap;
#[macro_use]
extern crate lazy_static;
extern crate pretty_bytes;
extern crate time;
extern crate chrono;
extern crate flate2;
extern crate filetime;
extern crate termcolor;
extern crate url;
extern crate iron;
extern crate multipart;
extern crate hyper_native_tls;
extern crate conduit_mime_types as mime_types;

mod util;
mod color;
mod middlewares;

use std::env;
use std::fs;
use std::cmp::Ordering;
use std::str::FromStr;
use std::net::IpAddr;
use std::io::{Read, Seek, SeekFrom};
use std::path::{PathBuf, Path};
use std::error::Error;
use std::collections::BTreeMap;

use iron::headers;
use iron::status;
use iron::method;
use iron::headers::{ContentEncoding, Encoding, AcceptEncoding, QualityItem};
use iron::modifiers::Redirect;
use iron::{Iron, Request, Response, IronResult, IronError, Set, Chain, Handler};
use multipart::server::{Multipart, SaveResult};
use pretty_bytes::converter::convert;
use termcolor::{Color, ColorSpec};
use url::percent_encoding::{percent_decode};

use util::{
    ROOT_LINK,
    StringError,
    enable_string, now_string, error_resp,
    system_time_to_date_time, encode_link_path, error_io2iron,
};
use color::{Printer, build_spec};

use middlewares::{AuthChecker, CompressionHandler, RequestLogger};

const ORDER_ASC: &'static str = "asc";
const ORDER_DESC: &'static str = "desc";
const DEFAULT_ORDER: &'static str = ORDER_DESC;

lazy_static! {
    static ref MIME_TYPES: mime_types::Types = mime_types::Types::new().unwrap();
    static ref SORT_FIELDS: Vec<&'static str> = vec!["name", "modified", "size"];
}

fn main() {
    let matches = clap::App::new("Simple HTTP(s) Server")
        .version(crate_version!())
        .arg(clap::Arg::with_name("root")
             .index(1)
             .validator(|s| {
                 match fs::metadata(s) {
                     Ok(metadata) => {
                         if metadata.is_dir() { Ok(()) } else {
                             Err("Not directory".to_owned())
                         }
                     },
                     Err(e) => Err(e.description().to_string())
                 }
             })
             .help("Root directory"))
        .arg(clap::Arg::with_name("index")
             .short("i")
             .long("index")
             .help("Enable automatic render index page [index.html, index.htm]"))
        .arg(clap::Arg::with_name("upload")
             .short("u")
             .long("upload")
             .help("Enable upload files (multiple select)"))
        .arg(clap::Arg::with_name("nosort")
             .long("nosort")
             .help("Disable directory entries sort (by: name, modified, size)"))
        .arg(clap::Arg::with_name("nocache")
             .long("nocache")
             .help("Disable http cache"))
        .arg(clap::Arg::with_name("norange")
             .long("norange")
             .help("Disable header::Range support (partial request)"))
        .arg(clap::Arg::with_name("cert")
             .long("cert")
             .takes_value(true)
             .validator(|s| {
                 match fs::metadata(s) {
                     Ok(metadata) => {
                         if metadata.is_file() { Ok(()) } else {
                             Err("Not a regular file".to_owned())
                         }
                     },
                     Err(e) => Err(e.description().to_string())
                 }
             })
             .help("TLS/SSL certificate (pkcs#12 format)"))
        .arg(clap::Arg::with_name("certpass").
             long("certpass")
             .takes_value(true)
             .help("TLS/SSL certificate password"))
        .arg(clap::Arg::with_name("ip")
             .long("ip")
             .takes_value(true)
             .default_value("0.0.0.0")
             .validator(|s| {
                 match IpAddr::from_str(&s) {
                     Ok(_) => Ok(()),
                     Err(e) => Err(e.description().to_string())
                 }
             })
             .help("IP address to bind"))
        .arg(clap::Arg::with_name("port")
             .short("p")
             .long("port")
             .takes_value(true)
             .default_value("8000")
             .validator(|s| {
                 match s.parse::<u16>() {
                     Ok(_) => Ok(()),
                     Err(e) => Err(e.description().to_string())
                 }
             })
             .help("Port number"))
        .arg(clap::Arg::with_name("auth")
             .short("a")
             .long("auth")
             .takes_value(true)
             .validator(|s| {
                 let parts = s.splitn(2, ':').collect::<Vec<&str>>();
                 if parts.len() < 2 || parts.len() >= 2 && parts[1].len() < 1 {
                     Err("no password found".to_owned())
                 } else if parts[0].len() < 1 {
                     Err("no username found".to_owned())
                 } else {
                     Ok(())
                 }
             })
             .help("HTTP Basic Auth (username:password)"))
        .arg(clap::Arg::with_name("compress")
             .short("c")
             .long("compress")
             .multiple(true)
             .value_delimiter(",")
             .takes_value(true)
             .help("Enable file compression: gzip/deflate\n    Example: -c=js,d.ts\n    Note: disabled on partial request!"))
        .arg(clap::Arg::with_name("threads")
             .short("t")
             .long("threads")
             .takes_value(true)
             .default_value("3")
             .validator(|s| {
                 match s.parse::<u8>() {
                     Ok(v) => {
                         if v > 0 { Ok(()) } else {
                             Err("Not positive number".to_owned())
                         }
                     }
                     Err(e) => Err(e.description().to_string())
                 }
             })
             .help("How many worker threads"))
        .get_matches();

    let root = matches
        .value_of("root")
        .map(|s| PathBuf::from(s))
        .unwrap_or(env::current_dir().unwrap());
    let index = matches.is_present("index");
    let upload = matches.is_present("upload");
    let sort = !matches.is_present("nosort");
    let cache = !matches.is_present("nocache");
    let range = !matches.is_present("norange");
    let cert = matches.value_of("cert");
    let certpass = matches.value_of("certpass");
    let ip = matches.value_of("ip").unwrap();
    let port = matches
        .value_of("port")
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let auth = matches.value_of("auth");
    let compress = matches.values_of_lossy("compress");
    let threads = matches
        .value_of("threads")
        .unwrap()
        .parse::<u8>()
        .unwrap();

    let printer = Printer::new();
    // TODO: may remove it later
    // if range && compress.is_some() {
    //     printer.println_err(
    //         "{}: Range and Compression can not both enabled! You may use `{}` to disable Range.", &vec![
    //             ("ERROR", &Some(build_spec(Some(Color::Red), true))),
    //             ("--norange", &Some(build_spec(Some(Color::Green), false)))
    //     ]).unwrap();
    //     std::process::exit(1);
    // }
    let color_blue = Some(build_spec(Some(Color::Blue), false));
    let addr = format!("{}:{}", ip, port);
    let compression_exts = compress.clone()
        .unwrap_or(Vec::new())
        .iter()
        .map(|s| format!("*.{}", s))
        .collect::<Vec<String>>();
    let compression_string = if compression_exts.is_empty() {
        "disabled".to_owned()
    } else {
        format!("{:?}", compression_exts)
    };
    printer.println_out(
        r#"  Index: {}, Upload: {}, Cache: {}, Range: {}, Sort: {}, Threads: {}
   Auth: {}, Compression: {}
  https: {}, Cert: {}, Cert-Password: {}
   Root: {}
Address: {}
======== [{}] ========"#,
        &vec![
            enable_string(index),
            enable_string(upload),
            enable_string(cache),
            enable_string(range),
            enable_string(sort),
            threads.to_string(),
            auth.unwrap_or("disabled").to_string(),
            compression_string,
            (if cert.is_some() { "enabled" } else { "disabled" }).to_string(),
            cert.unwrap_or("").to_owned(),
            certpass.unwrap_or("").to_owned(),
            root.to_str().unwrap().to_owned(),
            format!("{}://{}", if cert.is_some() {"https"} else {"http"}, addr),
            now_string()
        ].iter()
            .map(|s| (s.as_str(), &color_blue))
            .collect::<Vec<(&str, &Option<ColorSpec>)>>()
    ).unwrap();

    let mut chain = Chain::new(MainHandler{
        root, index, upload, cache, range, sort,
        compress: compress
            .clone()
            .map(|exts| exts
                 .iter()
                 .map(|s| format!(".{}", s))
                 .collect())
    });
    if let Some(auth) = auth {
        chain.link_before(AuthChecker::new(auth));
    }
    if let Some(ref exts) = compress {
        if !exts.is_empty() {
            chain.link_after(CompressionHandler);
        }
    }
    chain.link_after(RequestLogger{ printer: Printer::new() });
    let mut server = Iron::new(chain);
    server.threads = threads as usize;
    let rv = if let Some(cert) = cert {
        use hyper_native_tls::NativeTlsServer;
        let ssl = NativeTlsServer::new(cert, certpass.unwrap_or("")).unwrap();
        server.https(&addr, ssl)
    } else {
        server.http(&addr)
    };
    if let Err(e) = rv {
        printer.println_err("{}: Can not bind on {}, {}", &vec![
            ("ERROR", &Some(build_spec(Some(Color::Red), true))),
            (addr.as_str(), &None),
            (e.to_string().as_str(), &None)
        ]).unwrap();
        std::process::exit(1);
    };
}

struct MainHandler {
    root: PathBuf,
    index: bool,
    upload: bool,
    cache: bool,
    range: bool,
    sort: bool,
    compress: Option<Vec<String>>
}

impl Handler for MainHandler {
    fn handle(&self, req: &mut Request) -> IronResult<Response> {
        let mut fs_path = self.root.clone();
        let path_prefix = req.url.path()
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(|s| {
                percent_decode(s.as_bytes())
                    .decode_utf8().unwrap()
                    .to_string()
            })
            .collect::<Vec<String>>();
        for part in path_prefix.iter() {
            fs_path.push(part);
        }

        if self.upload && req.method == method::Post {
            if let Err((s, msg)) = self.save_files(req, &fs_path) {
                return Ok(error_resp(s, &msg));
            } else {
                return Ok(Response::with((status::Found, Redirect(req.url.clone()))))
            }
        }

        let path_metadata = try!(fs::metadata(&fs_path).map_err(error_io2iron));
        if path_metadata.is_dir() {
            self.list_directory(req, &fs_path, path_prefix)
        } else {
            self.send_file(req, &fs_path)
        }
    }
}

impl MainHandler {

    fn save_files(&self, req: &mut Request, path: &PathBuf) -> Result<(), (status::Status, String)> {
        match Multipart::from_request(req) {
            Ok(mut multipart) => {
                // Fetching all data and processing it.
                // save().temp() reads the request fully, parsing all fields and saving all files
                // in a new temporary directory under the OS temporary directory.
                match multipart.save().temp() {
                    SaveResult::Full(entries) => {
                        for (_, files) in entries.files {
                            for file in files {
                                let mut target_path = path.clone();
                                target_path.push(file.filename.clone().unwrap());
                                if let Err(errno) = fs::copy(file.path, target_path) {
                                    return Err((status::InternalServerError, format!("Copy file failed: {}", errno)));
                                } else {
                                    println!("  >> File saved: {}", file.filename.clone().unwrap());
                                }
                            }
                        }
                        Ok(())
                    },
                    SaveResult::Partial(_entries, reason) => {
                        Err((status::InternalServerError, reason.unwrap_err().description().to_owned()))
                    }
                    SaveResult::Error(error) => Err((status::InternalServerError, error.description().to_owned())),
                }
            }
            Err(_) => Err((status::BadRequest ,"The request is not multipart".to_owned()))
        }
    }

    fn list_directory(&self, req: &mut Request, fs_path: &PathBuf, path_prefix: Vec<String>) -> IronResult<Response> {

        struct Entry {
            filename: String,
            metadata: fs::Metadata
        }

        let mut resp = Response::with(status::Ok);
        let mut fs_path = fs_path.clone();
        let mut rows = Vec::new();

        let read_dir = try!(fs::read_dir(&fs_path).map_err(error_io2iron));
        let mut entries = Vec::new();
        for entry_result in read_dir {
            let entry = try!(entry_result.map_err(error_io2iron));
            entries.push(Entry{
                filename: entry.file_name().into_string().unwrap(),
                metadata: try!(entry.metadata().map_err(error_io2iron))
            });
        }

        // Breadcrumb navigation
        let breadcrumb = if path_prefix.len() > 0 {
            let mut breadcrumb = path_prefix.clone();
            let mut bread_links: Vec<String> = Vec::new();
            bread_links.push(breadcrumb.pop().unwrap().to_owned());
            while breadcrumb.len() > 0 {
                bread_links.push(format!(
                    r#"<a href="/{link}/"><strong>{label}</strong></a>"#,
                    link=encode_link_path(&breadcrumb), label=breadcrumb.pop().unwrap().to_owned(),
                ));
            }
            bread_links.push(ROOT_LINK.to_owned());
            bread_links.reverse();
            bread_links.join(" / ")
        } else { ROOT_LINK.to_owned() };

        // Sort links
        let sort_links = if self.sort {
            let mut sort_field = None;
            let mut order = None;
            for (k, v) in req.url.as_ref().query_pairs() {
                if k == "sort" {
                    sort_field = Some(v.to_string());
                } else if k == "order" {
                    order = Some(v.to_string());
                }
            }
            let order = order.unwrap_or(DEFAULT_ORDER.to_owned());
            let mut order_labels = BTreeMap::new();
            for field in SORT_FIELDS.iter() {
                if sort_field == Some((*field).to_owned()) && order == ORDER_DESC {
                    // reverse the order of the field
                    order_labels.insert(field.to_owned(), ORDER_ASC);
                }
            }

            if let Some(field) = sort_field {
                if SORT_FIELDS.iter().position(|s| *s == field.as_str()).is_none() {
                    return Err(IronError::new(
                        StringError(format!("Unknown sort field: {}", field)),
                        status::BadRequest));
                }
                if vec![ORDER_ASC, ORDER_DESC].iter().position(|s| *s == order).is_none() {
                    return Err(IronError::new(
                        StringError(format!("Unknown sort order: {}", order)),
                        status::BadRequest));
                }

                let reverse = order == ORDER_DESC;
                entries.sort_by(|a, b| {
                    let rv = match field.as_str() {
                        "name" => {
                            a.filename.cmp(&b.filename)
                        }
                        "modified" => {
                            let a = a.metadata.modified().unwrap();
                            let b = b.metadata.modified().unwrap();
                            a.cmp(&b)
                        }
                        "size" => {
                            if a.metadata.is_dir() == b.metadata.is_dir()
                                || a.metadata.is_file() == b.metadata.is_file() {
                                a.metadata.len().cmp(&b.metadata.len())
                            } else if a.metadata.is_dir() {
                                Ordering::Less
                            } else {
                                Ordering::Greater
                            }
                        }
                        _ => { unreachable!() }
                    };
                    if reverse { rv.reverse() } else { rv }
                });
            }

            let mut current_link = path_prefix.clone();
            current_link.push("".to_owned());
            format!(r#"
<tr>
  <th><a href="/{link}?sort=name&order={name_order}">Name</a></th>
  <th><a href="/{link}?sort=modified&order={modified_order}">Last modified</a></th>
  <th><a href="/{link}?sort=size&order={size_order}">Size</a></th>
</tr>
<tr><td style="border-top:1px dashed #BBB;" colspan="5"></td></tr>
"#,
                    link=encode_link_path(&current_link),
                    name_order=order_labels.get("name").unwrap_or(&DEFAULT_ORDER),
                    modified_order=order_labels.get("modified").unwrap_or(&DEFAULT_ORDER),
                    size_order=order_labels.get("size").unwrap_or(&DEFAULT_ORDER)
            )
        }  else { "".to_owned() };

        // Goto parent directory link
        if path_prefix.len() > 0 {
            let mut link = path_prefix.clone();
            link.pop();
            if link.len() > 0 {
                link.push("".to_owned());
            }
            rows.push(format!(
                r#"
<tr>
  <td><a href="/{link}"><strong>[Up]</strong></a></td>
  <td></td>
  <td></td>
</tr>
"#,
                link=encode_link_path(&link)
            ));
        } else {
            rows.push(r#"<tr><td>&nbsp;</td></tr>"#.to_owned());
        }

        // Directory entries
        for Entry{ filename, metadata } in entries {
            if self.index {
                for fname in vec!["index.html", "index.htm"] {
                    if filename == fname {
                        // Automatic render index page
                        fs_path.push(filename);
                        return self.send_file(req, &fs_path);
                    }
                }
            }
            // * Entry.modified
            let file_modified = system_time_to_date_time(metadata.modified().unwrap())
                .format("%Y-%m-%d %H:%M:%S").to_string();
            // * Entry.filesize
            let file_size = if metadata.is_dir() {
                "-".to_owned()
            } else {
                convert(metadata.len() as f64)
            };
            // * Entry.linkstyle
            let link_style = if metadata.is_dir() {
                "style=\"font-weight: bold;\"".to_owned()
            } else {
                "".to_owned()
            };
            // * Entry.link
            let mut link = path_prefix.clone();
            link.push(filename.clone());
            if metadata.is_dir() {
                link.push("".to_owned());
            }
            // * Entry.label
            let file_name_label = if metadata.is_dir() {
                format!("{}/", &filename)
            } else { filename.clone() };

            // Render one directory entry
            rows.push(format!(
                r#"
<tr>
  <td><a {linkstyle} href="/{link}">{label}</a></td>
  <td style="color:#888;">[{modified}]</td>
  <td><bold>{filesize}</bold></td>
</tr>
"#,
                linkstyle=link_style,
                link=encode_link_path(&link),
                label=file_name_label,
                modified=file_modified,
                filesize=file_size
            ));
        }

        // Optinal upload form
        let upload_form = if self.upload {
            format!(
                r#"
<form style="margin-top:1em; margin-bottom:1em;" action="/{path}" method="POST" enctype="multipart/form-data">
  <input type="file" name="files" accept="*" multiple />
  <input type="submit" value="Upload" />
</form>
"#,
                path=encode_link_path(&path_prefix))
        } else { "".to_owned() };

        // Put all parts together
        resp.set_mut(format!(
            r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <style> a {{ text-decoration:none; }} </style>
</head>
<body>
  {upload_form}
  <div>{breadcrumb}</div>
  <hr />
  <table>
    {sort_links}
    {rows}
  </table>
</body>
</html>
"#,
            upload_form=upload_form,
            breadcrumb=breadcrumb,
            sort_links=sort_links,
            rows=rows.join("\n")));

        resp.headers.set(headers::ContentType::html());
        if self.compress.is_some() {
            if let Some(&AcceptEncoding(ref encodings)) = req.headers.get::<AcceptEncoding>() {
                for &QualityItem{ ref item, ..} in encodings {
                    if *item == Encoding::Deflate || *item == Encoding::Gzip {
                        resp.headers.set(ContentEncoding(vec![Encoding::Gzip]));
                    }
                }
            }
        }
        Ok(resp)
    }

    fn send_file<P: AsRef<Path>>(&self, req: &Request, path: P) -> IronResult<Response> {
        use iron::headers::{IfModifiedSince, CacheControl, LastModified, CacheDirective, HttpDate};
        use iron::headers::{ContentLength, ContentType, ETag, EntityTag,
                            AcceptRanges, RangeUnit, Range, ByteRangeSpec, IfRange, IfMatch,
                            ContentRange, ContentRangeSpec};
        use iron::method::Method;
        use iron::mime::{Mime, TopLevel, SubLevel};
        use filetime::FileTime;

        let path = path.as_ref();
        let metadata = try!(fs::metadata(path).map_err(error_io2iron));

        let time = FileTime::from_last_modification_time(&metadata);
        let modified = time::Timespec::new(time.seconds() as i64, 0);
        let etag = EntityTag::weak(
            format!("{0:x}-{1:x}.{2:x}", metadata.len(), modified.sec, modified.nsec)
        );

        let mut resp = Response::with(status::Ok);
        if self.range {
            resp.headers.set(AcceptRanges(vec![RangeUnit::Bytes]));
        }
        match req.method {
            Method::Head => {
                let content_type = req.headers.get::<ContentType>()
                    .map(|t| t.clone())
                    .unwrap_or(ContentType(Mime(TopLevel::Text, SubLevel::Plain, vec![])));
                resp.headers.set(content_type);
                resp.headers.set(ContentLength(metadata.len()));
            },
            Method::Get => {
                // Set mime type
                let mime_str = MIME_TYPES.mime_for_path(path);
                let _ = mime_str.parse().map(|mime: Mime| resp.set_mut(mime));

                if self.range {
                    let mut range = req.headers.get::<Range>();

                    if range.is_some() {
                        // [Reference]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/If-Match
                        // Check header::If-Match
                        if let Some(&IfMatch::Items(ref items)) = req.headers.get::<IfMatch>() {
                            if items.iter().position(|item| item.strong_eq(&etag)).is_none() {
                                return Err(IronError::new(
                                    StringError("Etag not matched".to_owned()),
                                    status::RangeNotSatisfiable
                                ));
                            }
                        };
                    }

                    // [Reference]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/If-Range
                    let matched_ifrange = match req.headers.get::<IfRange>() {
                        Some(&IfRange::EntityTag(ref etag_ifrange)) => etag.weak_eq(etag_ifrange),
                        Some(&IfRange::Date(HttpDate(ref date_ifrange))) => &time::at(modified) <= date_ifrange,
                        None => true
                    };
                    if !matched_ifrange {
                        range = None;
                    }

                    match range {
                        Some(&Range::Bytes(ref ranges)) => {
                            if let Some(range) = ranges.get(0) {
                                let (offset, length) = match range {
                                    &ByteRangeSpec::FromTo(x, mut y) => { // "x-y"
                                        if x >= metadata.len() || x > y {
                                            return Err(IronError::new(
                                                StringError(format!("Invalid range(x={}, y={})", x, y)),
                                                status::RangeNotSatisfiable
                                            ));
                                        }
                                        if y >= metadata.len() {
                                            y = metadata.len() - 1;
                                        }
                                        (x, y - x + 1)
                                    }
                                    &ByteRangeSpec::AllFrom(x) => { // "x-"
                                        if x >= metadata.len() {
                                            return Err(IronError::new(
                                                StringError(format!(
                                                    "Range::AllFrom to large (x={}), Content-Length: {})",
                                                    x, metadata.len())),
                                                status::RangeNotSatisfiable
                                            ));
                                        }
                                        (x, metadata.len() - x)
                                    }
                                    &ByteRangeSpec::Last(mut x) => { // "-x"
                                        if x > metadata.len() {
                                            x = metadata.len();
                                        }
                                        (metadata.len() - x, x)
                                    }
                                };
                                let mut file = try!(fs::File::open(path).map_err(error_io2iron));
                                try!(file.seek(SeekFrom::Start(offset)).map_err(error_io2iron));
                                let take = file.take(length);

                                resp.headers.set(ContentLength(length));
                                resp.headers.set(ContentRange(ContentRangeSpec::Bytes{
                                    range: Some((offset, offset + length - 1)),
                                    instance_length: Some(metadata.len())
                                }));
                                resp.body = Some(Box::new(Box::new(take) as Box<Read + Send>));
                                resp.set_mut(status::PartialContent);
                            } else {
                                return Err(IronError::new(
                                    StringError("Empty range set".to_owned()),
                                    status::RangeNotSatisfiable
                                ));
                            }
                        }
                        Some(_) => {
                            return Err(IronError::new(
                                StringError("Invalid range type".to_owned()),
                                status::RangeNotSatisfiable
                            ));
                        }
                        _ => {
                            resp.headers.set(ContentLength(metadata.len()));
                            let file = try!(fs::File::open(path).map_err(error_io2iron));
                            resp.body = Some(Box::new(file));
                        }
                    }
                } else {
                    resp.headers.set(ContentLength(metadata.len()));
                    let file = try!(fs::File::open(path).map_err(error_io2iron));
                    resp.body = Some(Box::new(file));
                }
            }
            _ => { /* Should redirect to the same URL */ }
        }

        if let Some(ref exts) = self.compress {
            let path_str = path.to_string_lossy();
            if resp.status != Some(status::PartialContent) &&
                exts.iter().position(|ext| path_str.ends_with(ext)).is_some() {
                if let Some(&AcceptEncoding(ref encodings)) = req.headers.get::<AcceptEncoding>() {
                    for &QualityItem{ ref item, ..} in encodings {
                        if *item == Encoding::Deflate || *item == Encoding::Gzip {
                            resp.headers.set(ContentEncoding(vec![item.clone()]));
                            break;
                        }
                    }
                }
            }
        }

        if self.cache {
            static SECONDS: u32 = 5 * 60;
            if let Some(&IfModifiedSince(HttpDate(ref if_modified_since))) = req.headers.get::<IfModifiedSince>() {
                if modified <= if_modified_since.to_timespec() {
                    return Ok(Response::with(status::NotModified))
                }
            };
            let cache = vec![CacheDirective::Public, CacheDirective::MaxAge(SECONDS)];
            resp.headers.set(CacheControl(cache));
            resp.headers.set(LastModified(HttpDate(time::at(modified))));
            resp.headers.set(ETag(etag));
        }
        Ok(resp)
    }
}
