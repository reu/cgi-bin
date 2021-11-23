use clap::{clap_app, crate_description, crate_version};
use futures::stream::StreamExt;
use hyper::{
    header,
    http::{uri::Authority, StatusCode},
    server::conn::AddrStream,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use std::{convert::Infallible, process::Stdio};
use tokio::{
    io::{self, AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::io::{ReaderStream, StreamReader};

#[tokio::main]
async fn main() {
    let args = clap_app!(cgibin =>
        (version: crate_version!())
        (about: crate_description!())
        (@arg port: -p --port +takes_value "Port to run (defaults to 8080)")
    )
    .get_matches();

    let port = args
        .value_of("port")
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(8080_u16);

    let addr = format!("0.0.0.0:{}", port);

    Server::bind(&addr.parse().expect("Invalid socket address"))
        .serve(make_service_fn(|conn: &AddrStream| {
            let remote_addr = conn.remote_addr();
            async move {
                Ok::<_, Infallible>(service_fn(move |req: Request<Body>| async move {
                    let (script_name, path_info) = match req
                        .uri()
                        .path()
                        .splitn(3, '/')
                        .skip(1)
                        .collect::<Vec<&str>>()
                        .as_slice()
                    {
                        [script, path] if !(*path).is_empty() => {
                            (format!("/{}", script), format!("/{}", path))
                        }
                        [script, _] => (format!("/{}", script), String::default()),
                        [script] => (format!("/{}", script), String::default()),
                        _ => (String::default(), String::default()),
                    };

                    let script_path = std::fs::canonicalize(format!("./cgi-bin{}", script_name))
                        .expect("Invalid script");

                    let mut child = Command::new(&script_path)
                        .current_dir(std::fs::canonicalize("./cgi-bin").unwrap())
                        .env_clear()
                        .env("GATEWAY_INTERFACE", "CGI/1.1")
                        .env("QUERY_STRING", req.uri().query().unwrap_or_default())
                        .env("PATH_INFO", &path_info)
                        .env("PATH_TRANSLATED", &script_path)
                        .env("REQUEST_METHOD", req.method().as_str().to_ascii_uppercase())
                        .env("REMOTE_ADDR", remote_addr.ip().to_string())
                        .env("REMOTE_PORT", remote_addr.port().to_string())
                        .env("SCRIPT_NAME", &script_name)
                        .env(
                            "SERVER_NAME",
                            req.headers()
                                .get(header::HOST)
                                .and_then(|val| val.to_str().ok())
                                .and_then(|host| host.parse::<Authority>().ok())
                                .map(|authority| authority.host().to_owned())
                                .unwrap_or_default(),
                        )
                        .env("SERVER_PORT", port.to_string())
                        .env("SERVER_PROTOCOL", format!("{:?}", req.version()))
                        .env("SERVER_SOFTWARE", "cgi-bin/0.0.1")
                        .env(
                            "CONTENT_TYPE",
                            req.headers()
                                .get(header::CONTENT_TYPE)
                                .and_then(|val| val.to_str().ok())
                                .unwrap_or_default(),
                        )
                        .env(
                            "CONTENT_LENGTH",
                            req.headers()
                                .get(header::CONTENT_LENGTH)
                                .and_then(|val| val.to_str().ok())
                                .unwrap_or_default(),
                        )
                        .envs(req.headers().into_iter().map(|(name, value)| {
                            let name = format!("HTTP_{}", name)
                                .replace("-", "_")
                                .to_ascii_uppercase();
                            (name, value.to_str().unwrap())
                        }))
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::inherit())
                        .spawn()
                        .expect("Failed to run child process");

                    let mut stdin = child.stdin.take().expect("Failed to get process STDIN");
                    let stdout = child.stdout.take().expect("Failed to get process STDOUT");

                    let write_request_body = async move {
                        let request_body = req.into_body().map(|chunk| {
                            chunk.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))
                        });
                        let mut request_body_reader = StreamReader::new(request_body);
                        io::copy(&mut request_body_reader, &mut stdin)
                            .await
                            .expect("Failed to write to STDIN");
                    };

                    let read_response = async move {
                        let mut stdout_reader = BufReader::new(stdout);
                        let mut headers = Vec::new();

                        loop {
                            stdout_reader
                                .read_until(b'\n', &mut headers)
                                .await
                                .expect("Failed to read from STDOUT");

                            match headers.as_slice() {
                                [.., b'\r', b'\n', b'\r', b'\n'] => break,
                                [.., b'\n', b'\n'] => break,
                                _ => continue,
                            }
                        }

                        let mut parsed_headers = Vec::with_capacity(8);
                        httparse::parse_headers(&headers, &mut parsed_headers)
                            .expect("Failed to parse headers");

                        let response = parsed_headers
                            .into_iter()
                            .filter(|header| *header != httparse::EMPTY_HEADER)
                            .map(|header| (header.name, header.value))
                            .fold(Response::builder(), |response, (name, value)| {
                                if name.to_ascii_lowercase() == "status" {
                                    response.status(
                                        StatusCode::from_bytes(&value[0..3])
                                            .expect("Invalid status code"),
                                    )
                                } else {
                                    response.header(name, value)
                                }
                            });

                        response
                            .body(Body::wrap_stream(ReaderStream::new(stdout_reader)))
                            .expect("Invalid response")
                    };

                    let (_, response) = tokio::join!(write_request_body, read_response);

                    tokio::spawn(async move {
                        child.wait().await.expect("Process exited with an error");
                    });

                    Ok::<_, Infallible>(response)
                }))
            }
        }))
        .await
        .unwrap();
}
