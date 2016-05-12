// Copyright 2016 The Rustw Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use build;
use build::errors::{self, Diagnostic};
use config::Config;
use file_cache::{Cache, DirectoryListing};
use reprocess;

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write, BufRead};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::{self, sleep};
use std::time;

use hyper::header::ContentType;
use hyper::net::Fresh;
use hyper::server::request::Request;
use hyper::server::response::Response;
use hyper::status::StatusCode;
use hyper::uri::RequestUri;
use serde_json;
use url::parse_path;

/// An instance of the server. Runs a session of rustw.
pub struct Instance {
    builder: build::Builder,
    pub config: Arc<Config>,
    file_cache: Arc<Mutex<Cache>>,
    pending_push_data: Arc<Mutex<HashMap<String, Option<String>>>>,
}

impl Instance {
    pub fn new(config: Config) -> Instance {
        let config = Arc::new(config);
        Instance {
            builder: build::Builder::from_config(config.clone()),
            config: config,
            file_cache: Arc::new(Mutex::new(Cache::new())),
            // FIXME(#58) a rebuild should cancel all pending tasks.
            pending_push_data: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl ::hyper::server::Handler for Instance {
    fn handle<'a, 'k>(&'a self, req: Request<'a, 'k>, res: Response<'a, Fresh>) {
        let uri = req.uri.clone();
        if let RequestUri::AbsolutePath(ref s) = uri {
            let mut handler = Handler {
                config: &self.config,
                builder: &self.builder,
                file_cache: &self.file_cache,
                pending_push_data: &self.pending_push_data,
            };
            route(s, &mut handler, req, res);
        } else {
            // TODO log this and ignore it.
            panic!("Unexpected uri");
        }
    }
}

// Handles a single request.
struct Handler<'a> {
    pub config: &'a Arc<Config>,
    builder: &'a build::Builder,
    file_cache: &'a Arc<Mutex<Cache>>,
    pending_push_data: &'a Arc<Mutex<HashMap<String, Option<String>>>>,
}

impl<'a> Handler<'a> {
    fn handle_error<'b: 'a, 'k: 'a>(&self,
                                    _req: Request<'b, 'k>,
                                    mut res: Response<'b, Fresh>,
                                    status: StatusCode,
                                    msg: String) {
        // TODO log it
        //println!("ERROR: {} ({})", msg, status);

        *res.status_mut() = status;
        res.send(msg.as_bytes()).unwrap();        
    }

    fn handle_index<'b: 'a, 'k: 'a>(&mut self,
                                    _req: Request<'b, 'k>,
                                    mut res: Response<'b, Fresh>) {
        let mut path_buf = static_path();
        path_buf.push("index.html");

        let mut file_cache = self.file_cache.lock().unwrap();
        let msg = match file_cache.get_text(&path_buf) {
            Ok(data) => {
                res.headers_mut().set(ContentType::html());
                res.send(data).unwrap();
                return;
            }
            Err(s) => s,
        };

        self.handle_error(_req, res, StatusCode::InternalServerError, msg);
    }

    fn handle_static<'b: 'a, 'k: 'a>(&mut self,
                                     req: Request<'b, 'k>,
                                     mut res: Response<'b, Fresh>,
                                     path: &[String]) {
        let mut path_buf = static_path();
        for p in path {
            path_buf.push(p);
        }
        //println!("requesting `{}`", path_buf.to_str().unwrap());

        let content_type = match path_buf.extension() {
            Some(s) if s.to_str().unwrap() == "html" => ContentType::html(),
            Some(s) if s.to_str().unwrap() == "css" => ContentType("text/css".parse().unwrap()),
            Some(s) if s.to_str().unwrap() == "json" => ContentType::json(),
            _ => ContentType("application/octet-stream".parse().unwrap()),
        };

        let mut file_cache = self.file_cache.lock().unwrap();
        if let Ok(s) = file_cache.get_text(&path_buf) {
            //println!("serving `{}`. {} bytes, {}", path_buf.to_str().unwrap(), s.len(), content_type);
            res.headers_mut().set(content_type);
            res.send(s).unwrap();
            return;
        }        

        self.handle_error(req, res, StatusCode::NotFound, "Page not found".to_owned());
    }

    fn handle_src<'b: 'a, 'k: 'a>(&mut self,
                                  _req: Request<'b, 'k>,
                                  mut res: Response<'b, Fresh>,
                                  mut path: &[String]) {
        for p in path {
            // In demo mode this might reveal the contents of the server outside
            // the source directory (really, rustw should run in a sandbox, but
            // hey).
            if p.contains("..") {
                self.handle_error(_req, res, StatusCode::InternalServerError,
                                  "Bad path, found `..`".to_owned());
                return
            }
        }

        let mut path_buf = PathBuf::new();
        if path[0].is_empty() {
            path_buf.push("/");
            path = &path[1..];
        }
        for p in path {
            path_buf.push(p);
        }

        // TODO should cache directory listings too
        if path_buf.is_dir() {
            match DirectoryListing::from_path(&path_buf) {
                Ok(listing) => {
                    res.headers_mut().set(ContentType::json());
                    res.send(serde_json::to_string(&SourceResult::Directory(listing)).unwrap().as_bytes()).unwrap();
                }
                Err(msg) => self.handle_error(_req, res, StatusCode::InternalServerError, msg),
            }
        } else {
            let mut file_cache = self.file_cache.lock().unwrap();
            match file_cache.get_highlighted(&path_buf) {
                Ok(ref lines) => {
                    res.headers_mut().set(ContentType::json());
                    let result = SourceResult::Source {
                        path: path_buf.components().map(|c| c.as_os_str().to_str().unwrap().to_owned()).collect(),
                        lines: lines,
                    };
                    res.send(serde_json::to_string(&result).unwrap().as_bytes()).unwrap();
                }
                Err(msg) => self.handle_error(_req, res, StatusCode::InternalServerError, msg),
            }
        }
    }

    fn handle_config<'b: 'a, 'k: 'a>(&mut self,
                                     _req: Request<'b, 'k>,
                                     mut res: Response<'b, Fresh>) {
        let text = serde_json::to_string(&self.config).unwrap();

        res.headers_mut().set(ContentType::json());
        res.send(text.as_bytes()).unwrap();
    }

    fn handle_test<'b: 'a, 'k: 'a>(&mut self,
                                   _req: Request<'b, 'k>,
                                   mut res: Response<'b, Fresh>) {
        let build_result = build::BuildResult::test_result();
        let result = self.make_build_result(&build_result);
        let text = serde_json::to_string(&result).unwrap();

        res.headers_mut().set(ContentType::json());
        res.send(text.as_bytes()).unwrap();

        // TODO mock analysis
        self.process_push_data(result, vec![]);
    }

    fn handle_build<'b: 'a, 'k: 'a>(&mut self,
                                    _req: Request<'b, 'k>,
                                    mut res: Response<'b, Fresh>) {
        assert!(!self.config.demo_mode, "Build shouldn't happen in demo mode");

        {
            let mut file_cache = self.file_cache.lock().unwrap();
            file_cache.reset();
        }

        let build_result = self.builder.build().unwrap();
        let result = self.make_build_result(&build_result);
        let text = serde_json::to_string(&result).unwrap();

        res.headers_mut().set(ContentType::json());
        res.send(text.as_bytes()).unwrap();

        self.process_push_data(result, build_result.analysis);
    }

    fn make_build_result(&mut self, build_result: &build::BuildResult) -> BuildResult {
        let mut result = BuildResult::from_build(&build_result);
        if !result.errors.is_empty() {
            let key = reprocess::make_key();
            result.push_data_key = Some(key.clone());
            let mut pending_push_data = self.pending_push_data.lock().unwrap();
            pending_push_data.insert(key, None);
        }

        result        
    }

    fn process_push_data(&self, result: BuildResult, analysis: Vec<build::Analysis>) {
        if result.push_data_key.is_some() {
            let pending_push_data = self.pending_push_data.clone();
            let file_cache = self.file_cache.clone();
            let config = self.config.clone();
            thread::spawn(|| reprocess::reprocess_snippets(result, pending_push_data, analysis, file_cache, config));
        }        
    }

    fn handle_quick_edit<'b: 'a, 'k: 'a>(&mut self,
                                         mut req: Request<'b, 'k>,
                                         mut res: Response<'b, Fresh>) {
        assert!(!self.config.demo_mode, "Quick edit shouldn't happen in demo mode");

        res.headers_mut().set(ContentType::json());

        let mut buf = String::new();
        req.read_to_string(&mut buf).unwrap();
        if let Err(msg) = quick_edit(serde_json::from_str(&buf).unwrap()) {
            *res.status_mut() = StatusCode::InternalServerError;
            res.send(format!("{{ \"message\": \"{}\" }}", msg).as_bytes()).unwrap();
            return;
        }

        res.send("{}".as_bytes()).unwrap();                    
    }

    fn handle_edit<'b: 'a, 'k: 'a>(&mut self,
                                   _req: Request<'b, 'k>,
                                   mut res: Response<'b, Fresh>,
                                   query: Option<String>) {
        assert!(!self.config.demo_mode, "Edit shouldn't happen in demo mode");

        // TODO factor out query logic
        match query {
            Some(ref q) => {
                // Extract the `file` value from the query string.
                let start = match q.find("file=") {
                    Some(i) => i + 5,  // 5 = "file=".len()
                    None => {
                        self.handle_error(_req, res, StatusCode::InternalServerError, format!("Bad query string: {:?}", query));
                        return;
                    }
                };
                let end = q[start..].find("&").unwrap_or(q.len());

                // Split the 'filename' on colons for line and column numbers.
                let args = parse_location_string(&q[start..end]);

                let cmd_line = &self.config.edit_command;
                if !cmd_line.is_empty() {
                    let cmd_line = cmd_line.replace("$file", &args[0])
                                           .replace("$line", &args[1])
                                           .replace("$col", &args[2]);

                    let mut splits = cmd_line.split(' ');

                    let mut cmd = Command::new(splits.next().unwrap());
                    for arg in splits {
                        cmd.arg(arg);
                    }

                    // TODO log, don't print
                    match cmd.spawn() {
                        Ok(_) => println!("edit, launched successfully"),
                        Err(e) => println!("edit, launch failed: `{:?}`, command: `{}`", e, cmd_line),
                    }
                }

                res.headers_mut().set(ContentType::json());
                res.send("{}".as_bytes()).unwrap();
            }
            None => {
                self.handle_error(_req, res, StatusCode::InternalServerError, format!("Bad query string: {:?}", query));
            }
        }
    }

    fn handle_pull<'b: 'a, 'k: 'a>(&mut self,
                                   _req: Request<'b, 'k>,
                                   mut res: Response<'b, Fresh>,
                                   query: Option<String>) {
        match query {
            Some(ref q) => {
                // Extract the `key` value from the query string.
                let start = match q.find("key=") {
                    Some(i) => i + 4,  // 5 = "key=".len()
                    None => {
                        self.handle_error(_req, res, StatusCode::InternalServerError, format!("Bad query string: {:?}", query));
                        return;
                    }
                };
                let end = q[start..].find("&").unwrap_or(q.len());
                let key = &q[start..end];

                res.headers_mut().set(ContentType::json());

                loop {
                    let pending_push_data = self.pending_push_data.lock().unwrap();
                    match pending_push_data.get(key) {
                        Some(&Some(ref s)) => {
                            // Data is ready, return it.
                            res.send(s.as_bytes()).unwrap();
                            return;
                        }
                        Some(&None) => {
                            // Task is in progress, wait.
                        }
                        None => {
                            // No push task, return nothing.
                            res.send("{}".as_bytes()).unwrap();
                            return;
                        }
                    }
                    sleep(time::Duration::from_millis(200));
                }
            }
            None => {
                self.handle_error(_req, res, StatusCode::InternalServerError, format!("Bad query string: {:?}", query));
            }
        }
    }
}

#[derive(Serialize, Debug)]
pub enum SourceResult<'a> {
    Source{
        path: Vec<String>,
        lines: &'a [String],
    },
    Directory(DirectoryListing),
}

#[derive(Serialize, Debug)]
pub struct BuildResult {
    pub messages: String,
    pub errors: Vec<Diagnostic>,
    pub push_data_key: Option<String>,
    // build_command: String,
}

impl BuildResult {
    fn from_build(build: &build::BuildResult) -> BuildResult {
        BuildResult {
            messages: build.stdout.to_owned(),
            errors: errors::parse_errors(&build.stderr),
            push_data_key: None,
        }
    }
}

fn static_path() -> PathBuf {
    const STATIC_DIR: &'static str = "static";

    let mut result = ::std::env::current_exe().unwrap();
    assert!(result.pop());
    result.push(STATIC_DIR);
    result
}

pub fn parse_location_string(input: &str) -> [String; 5] {
    let mut args = input.split(':').map(|s| s.to_owned());
    [args.next().unwrap(),
     args.next().unwrap_or(String::new()),
     args.next().unwrap_or(String::new()),
     args.next().unwrap_or(String::new()),
     args.next().unwrap_or(String::new())]
}

fn read_lines(file: &File) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut reader = BufReader::new(file);

    loop {
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => return Ok(result),
            Ok(_) => result.push(buf),
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[derive(Deserialize, Debug)]
struct QuickEditData {
    file_name: String,
    line_start: usize,
    line_end: usize,
    text: String,
}

// FIXME there may well be a better place for this functionality.
fn quick_edit(data: QuickEditData) -> Result<(), String> {
    // TODO all these unwraps should return Err instead.

    // TODO we should check that the file has not been modified since we read it,
    // otherwise the file line locations will be incorrect.

    // Scope is so we close file after reading.
    let lines = {
        let file = match File::open(&data.file_name) {
            Ok(f) => f,
            Err(e) => return Err(e.to_string()),
        };

        read_lines(&file)?
    };

    assert!(data.line_start <= data.line_end && data.line_end <= lines.len());

    let file = File::create(&data.file_name).unwrap();
    let mut writer = BufWriter::new(file);

    for i in 0..(data.line_start - 1) {
        writer.write(lines[i].as_bytes()).unwrap();
    }
    writer.write(data.text.as_bytes()).unwrap();
    writer.write(&['\n' as u8]).unwrap();
    for i in data.line_end..lines.len() {
        writer.write(lines[i].as_bytes()).unwrap();
    }

    writer.flush().unwrap();
    Ok(())
}

const STATIC_REQUEST: &'static str = "static";
const SOURCE_REQUEST: &'static str = "src";
const CONFIG_REQUEST: &'static str = "config";
const BUILD_REQUEST: &'static str = "build";
const TEST_REQUEST: &'static str = "test";
const EDIT_REQUEST: &'static str = "edit";
const PULL_REQUEST: &'static str = "pull";
const QUICK_EDIT_REQUEST: &'static str = "quick_edit";

fn route<'a, 'b: 'a, 'k: 'a>(uri_path: &str,
                             handler: &'a mut Handler<'a>,
                             req: Request<'b, 'k>,
                             res: Response<'b, Fresh>) {
    let (path, query, _) = parse_path(uri_path).unwrap();

    //println!("path: {:?}, query: {:?}", path, query);
    if path.is_empty() || (path.len() == 1 && (path[0] == "index.html" || path[0] == "")) {
        handler.handle_index(req, res);
        return;
    }

    if path[0] == STATIC_REQUEST {
        handler.handle_static(req, res, &path[1..]);
        return;
    }

    if path[0] == CONFIG_REQUEST {
        handler.handle_config(req, res);
        return;
    }

    if path[0] == PULL_REQUEST {
        handler.handle_pull(req, res, query);
        return;
    }

    if path[0] == SOURCE_REQUEST {
        handler.handle_src(req, res, &path[1..]);
        return;
    }

    if path[0] == TEST_REQUEST {
        handler.handle_test(req, res);
        return;
    }

    if handler.config.demo_mode == false {
        if path[0] == BUILD_REQUEST {
            handler.handle_build(req, res);
            return;
        }

        if path[0] == EDIT_REQUEST {
            handler.handle_edit(req, res, query);
            return;
        }

        if path[0] == QUICK_EDIT_REQUEST {
            handler.handle_quick_edit(req, res);
            return;
        }
    }

    handler.handle_error(req, res, StatusCode::NotFound, format!("Unexpected path: `/{}`", path.join("/")));
}
