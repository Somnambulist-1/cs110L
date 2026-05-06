mod request;
mod response;

use clap::Parser;
use parking_lot::Mutex;
use rand::{Rng, SeedableRng};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Contains information parsed from the command-line invocation of balancebeam. The Clap macros
/// provide a fancy way to automatically construct a command-line argument parser.
#[derive(Parser, Debug)]
#[clap(about = "Fun with load balancing")]
struct CmdOptions {
    #[clap(
        short,
        long,
        help = "IP/port to bind to",
        default_value = "0.0.0.0:1100"
    )]
    bind: String,
    #[clap(short, long, help = "Upstream host to forward requests to")]
    upstream: Vec<String>,
    #[clap(
        long,
        help = "Perform active health checks on this interval (in seconds)",
        default_value = "10"
    )]
    active_health_check_interval: usize,
    #[clap(
        long,
        help = "Path to send request to for active health checks",
        default_value = "/"
    )]
    active_health_check_path: String,
    #[clap(
        long,
        help = "Maximum number of requests to accept per IP per minute (0 = unlimited)",
        default_value = "0"
    )]
    max_requests_per_minute: usize,
}

/// Contains information about the state of balancebeam (e.g. what servers we are currently proxying
/// to, what servers have failed, rate limiting counts, etc.)
///
/// You should add fields to this struct in later milestones.
struct ProxyState {
    /// How frequently we check whether upstream servers are alive (Milestone 4)
    #[allow(dead_code)]
    active_health_check_interval: usize,
    /// Where we should send requests when doing active health checks (Milestone 4)
    #[allow(dead_code)]
    active_health_check_path: String,
    /// Maximum number of requests an individual IP can make in a minute (Milestone 5)
    #[allow(dead_code)]
    max_requests_per_minute: usize,
    /// Addresses of servers that we are proxying to
    upstream_addresses: Vec<String>,
    /// Upstream indices that failed a passive connect check
    dead_upstreams: Mutex<HashSet<usize>>,
    /// Number of requests within a window
    request_counts: Mutex<HashMap<String, VecDeque<Instant>>>,
}

fn main() {
    // Initialize the logging library. You can print log messages using the `log` macros:
    // https://docs.rs/log/0.4.8/log/ You are welcome to continue using print! statements; this
    // just looks a little prettier.
    if let Err(_) = std::env::var("RUST_LOG") {
        std::env::set_var("RUST_LOG", "debug");
    }
    pretty_env_logger::init();

    // Parse the command line arguments passed to this program
    let options = CmdOptions::parse();
    if options.upstream.len() < 1 {
        log::error!("At least one upstream server must be specified using the --upstream option.");
        std::process::exit(1);
    }

    // Start listening for connections
    let listener = match TcpListener::bind(&options.bind) {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("Could not bind to {}: {}", options.bind, err);
            std::process::exit(1);
        }
    };
    log::info!("Listening for requests on {}", options.bind);

    // Handle incoming connections
    let state = Arc::new(ProxyState {
        upstream_addresses: options.upstream,
        active_health_check_interval: options.active_health_check_interval,
        active_health_check_path: options.active_health_check_path,
        max_requests_per_minute: options.max_requests_per_minute,
        dead_upstreams: Mutex::new(HashSet::new()),
        request_counts: Mutex::new(HashMap::new()),
    });

    let health_state = Arc::clone(&state);
    thread::spawn(move || {
        active_health_check(health_state);
    });

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            // Handle the connection!
            handle_connection(stream, &state);
        }
    }
}

fn connect_to_upstream(state: &ProxyState) -> Result<TcpStream, std::io::Error> {
    let mut rng = rand::rngs::StdRng::from_entropy();
    let mut tried_upstreams = HashSet::new();
    let mut last_error = None;

    loop {
        let candidates: Vec<usize> = {
            let dead_upstreams = state.dead_upstreams.lock();
            (0..state.upstream_addresses.len())
                .filter(|idx| !dead_upstreams.contains(idx) && !tried_upstreams.contains(idx))
                .collect()
        };

        if candidates.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "all upstream servers are dead")
            }));
        }

        let upstream_idx = candidates[rng.gen_range(0, candidates.len())];
        tried_upstreams.insert(upstream_idx);
        let upstream_ip = &state.upstream_addresses[upstream_idx];
        match TcpStream::connect(upstream_ip) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                log::error!("Failed to connect to upstream {}: {}", upstream_ip, err);
                state.dead_upstreams.lock().insert(upstream_idx);
                last_error = Some(err);
            }
        }
    }
}

fn send_response(client_conn: &mut TcpStream, response: &http::Response<Vec<u8>>) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!(
        "{} <- {}",
        client_ip,
        response::format_response_line(&response)
    );
    if let Err(error) = response::write_to_stream(&response, client_conn) {
        log::warn!("Failed to send response to client: {}", error);
        return;
    }
}

fn handle_connection(mut client_conn: TcpStream, state: &ProxyState) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!("Connection received from {}", client_ip);

    // Open a connection to a random destination server
    let mut upstream_conn = match connect_to_upstream(state) {
        Ok(stream) => stream,
        Err(_error) => {
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response);
            return;
        }
    };
    let upstream_ip = client_conn.peer_addr().unwrap().ip().to_string();

    // The client may now send us one or more requests. Keep trying to read requests until the
    // client hangs up or we get an error.
    loop {
        // Read a request from the client
        let mut request = match request::read_from_stream(&mut client_conn) {
            Ok(request) => request,
            // Handle case where client closed connection and is no longer sending requests
            Err(request::Error::IncompleteRequest(0)) => {
                log::debug!("Client finished sending requests. Shutting down connection");
                return;
            }
            // Handle I/O error in reading from the client
            Err(request::Error::ConnectionError(io_err)) => {
                log::info!("Error reading request from client stream: {}", io_err);
                return;
            }
            Err(error) => {
                log::debug!("Error parsing request: {:?}", error);
                let response = response::make_http_error(match error {
                    request::Error::IncompleteRequest(_)
                    | request::Error::MalformedRequest(_)
                    | request::Error::InvalidContentLength
                    | request::Error::ContentLengthMismatch => http::StatusCode::BAD_REQUEST,
                    request::Error::RequestBodyTooLarge => http::StatusCode::PAYLOAD_TOO_LARGE,
                    request::Error::ConnectionError(_) => http::StatusCode::SERVICE_UNAVAILABLE,
                });
                send_response(&mut client_conn, &response);
                continue;
            }
        };

        // rate limiting
        if is_rate_limited(state, &client_ip) {
            let response = response::make_http_error(http::StatusCode::TOO_MANY_REQUESTS);
            send_response(&mut client_conn, &response);
            return;
        }

        log::info!(
            "{} -> {}: {}",
            client_ip,
            upstream_ip,
            request::format_request_line(&request)
        );

        // Add X-Forwarded-For header so that the upstream server knows the client's IP address.
        // (We're the ones connecting directly to the upstream server, so without this header, the
        // upstream server will only know our IP, not the client's.)
        request::extend_header_value(&mut request, "x-forwarded-for", &client_ip);

        // Forward the request to the server
        if let Err(error) = request::write_to_stream(&request, &mut upstream_conn) {
            log::error!(
                "Failed to send request to upstream {}: {}",
                upstream_ip,
                error
            );
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response);
            return;
        }
        log::debug!("Forwarded request to server");

        // Read the server's response
        let response = match response::read_from_stream(&mut upstream_conn, request.method()) {
            Ok(response) => response,
            Err(error) => {
                log::error!("Error reading response from server: {:?}", error);
                let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
                send_response(&mut client_conn, &response);
                return;
            }
        };
        // Forward the response to the client
        send_response(&mut client_conn, &response);
        log::debug!("Forwarded response to client");
    }
}

fn is_rate_limited(state: &ProxyState, client_ip: &str) -> bool {
    if state.max_requests_per_minute == 0 {
        return false;
    }

    let now = Instant::now();
    let window = Duration::from_secs(60);

    let mut request_counts = state.request_counts.lock();
    let timestamps = request_counts
        .entry(client_ip.to_string())
        .or_insert_with(VecDeque::new);
    
    while let Some(timestamp) = timestamps.front() {
        if now.duration_since(*timestamp) > window {
            timestamps.pop_front();
        } else {
            break;
        }
    }

    if timestamps.len() >= state.max_requests_per_minute {
        return true;
    }

    timestamps.push_back(now);
    false
}

fn active_health_check(state: Arc<ProxyState>) {
    loop {
        thread::sleep(Duration::from_secs(
            state.active_health_check_interval as u64,
        ));

        for upstream_idx in 0..state.upstream_addresses.len() {
            let upstream_ip = &state.upstream_addresses[upstream_idx];

            let is_healthy = check_upstream_health(upstream_ip, &state.active_health_check_path);

            let mut dead_upstreams = state.dead_upstreams.lock();
            if is_healthy {
                dead_upstreams.remove(&upstream_idx);
            } else {
                dead_upstreams.insert(upstream_idx);
            }
        }
    }
}

fn check_upstream_health(upstream_ip: &str, path: &str) -> bool {
    let mut stream = match TcpStream::connect(upstream_ip) {
        Ok(stream) => stream,
        Err(_) => return false,
    };

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(path)
        .header("host", upstream_ip)
        .body(Vec::new())
        .unwrap();

    if request::write_to_stream(&request, &mut stream).is_err() {
        return false;
    }

    let response = match response::read_from_stream(&mut stream, request.method()) {
        Ok(response) => response,
        Err(_) => return false,
    };

    response.status() == http::StatusCode::OK
}
