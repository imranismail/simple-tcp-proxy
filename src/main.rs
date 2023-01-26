use futures::{AsyncReadExt, AsyncWriteExt, future, StreamExt};
use serde::{Deserialize, Serialize};
use async_std::{
    fs,
    io,
    task,
    net::{TcpListener, TcpStream}
};
use std::{
    sync::{Arc, Mutex},
    collections::HashMap,
    net::SocketAddr
};

#[derive(Debug, Deserialize, Serialize, Clone)]
struct App {
    #[serde(rename(deserialize  = "name"))]
    name: String,
    #[serde(rename(deserialize  = "ports"))]
    ports: Vec<u16>,
    #[serde(rename(deserialize  = "targets"))]
    targets: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Config {
    #[serde(rename(deserialize  = "apps"))]
    apps: Vec<App>,
}

impl Config {
    // Parses the specified yaml config file and returns a Config struct.
    async fn parse_config(filename: &str) -> io::Result<Self> {
        let contents = fs::read_to_string(filename).await?;
        let config: Config = serde_yaml::from_str(&contents).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(config)
    }
}

trait LoadBalancer: Send {
    // Creates a new instance of the load balancer.
    fn new() -> Arc<Mutex<Self>> where Self: Sized;

    // Chooses the next available target for the specified app.
    // If there are no available targets, returns None.
    fn choose_target(&mut self, app: &App) -> Option<String>;
}

// A load balancing algorithm that chooses the next available target using round-robin.
struct RoundRobin {
  // A map from app names to the next available target index for that app.
  next_target: HashMap<String, usize>,
}

impl LoadBalancer for RoundRobin {
    // Creates a new instance of the load balancer.
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(
            Mutex::new(
                RoundRobin {
                    next_target: HashMap::new(),
                }
            )
        )
    }

    // Chooses the next available target for the specified app.
    // If there are no available targets, returns None.
    fn choose_target(&mut self, app: &App) -> Option<String> {
        let name = app.name.clone();
        let targets = app.targets.clone();

        // If this is the first time we are choosing a target for this app,
        // start with the first target in the list.
        let index = self.next_target.entry(name).or_insert(0);

        // Increment the index and wrap around if necessary.
        *index += 1;

        // If we have reached the end of the list, wrap around to the beginning.
        if *index >= targets.len() {
            *index = 0;
        }

        // Return the next available target.
        Some(targets[*index].clone())
    }
}

struct Proxy {
    config: Config,
    load_balancer: Arc<Mutex<dyn LoadBalancer>>,
}

impl Proxy {
    async fn handle_connection(mut src: TcpStream, mut dst: TcpStream) {
        // Create bi-directional copies of the data between the src and the dst.
        let (mut src_reader, mut src_writer) = src.clone().split();
        let (mut dst_reader, mut dst_writer) = dst.clone().split();
        task::spawn(async move {
            io::copy(&mut src_reader, &mut dst_writer).await.expect("failed to copy from src to dst");
        });
        io::copy(&mut dst_reader, &mut src_writer).await.expect("failed to copy from dst to src");

        // Gracefully close the connection.
        src.close().await.expect("failed to close src connection");
        dst.close().await.expect("failed to close dst connection");

        // Print a message when the connection is closed.
        println!("{} -> {} closed", src.peer_addr().unwrap(), dst.peer_addr().unwrap());
    }

    // Starts listening on the specified ports and routing connections to the correct targets.
    async fn run(self) -> io::Result<()> {
        // Tasks that are currently running.
        let mut tasks = Vec::new();

        // Listen for incoming connections on each of the specified ports.
        for app in self.config.clone().apps.iter() {
            for port in app.ports.iter() {
                // Listen on the specified port.
                let addr = SocketAddr::from(([0, 0, 0, 0], *port));
                let listener = TcpListener::bind(addr).await?;
                println!("{} listening on: {}", app.name, listener.local_addr().unwrap());

                // Clone the app so that it can be moved into the async task below.
                let app = app.clone();

                // Clone the load balancer so that it can be moved into the async task below.
                let load_balancer = self.load_balancer.clone();

                // Accept connections and route them to the correct target.
                tasks.push(
                    task::spawn(async move {
                        // Accept incoming connections concurrently
                        listener.incoming().for_each_concurrent(None, |src| async {
                            let target = load_balancer.lock().unwrap().choose_target(&app).unwrap();

                            // Handle src connection errors.
                            let src = match src {
                                Ok(src) => src,
                                Err(e) => {
                                    println!("failed to accept connection: {}", e);
                                    return;
                                }
                            };

                            // Connect to the target.
                            let dst = match TcpStream::connect(&target).await {
                                Ok(dst) => dst,
                                Err(e) => {
                                    let target = load_balancer.lock().unwrap().choose_target(&app).unwrap();
                                    println!("failed to connect to {}: {}", target, e);
                                    return;
                                }
                            };
                            println!("{} -> {} -> {}", src.peer_addr().unwrap(), &target, dst.peer_addr().unwrap());

                            // Handle the connection.
                            Proxy::handle_connection(src, dst).await;
                        }).await;
                    })
                )
            }
        }

        // Wait for all tasks to complete.
        future::select_all(tasks).await;

        Ok(())
    }
}

#[async_std::main]
async fn main() -> io::Result<()> {
    let config = Config::parse_config("config.yaml").await?;
    let load_balancer = RoundRobin::new();
    let proxy = Proxy {
        config,
        load_balancer,
    };
    proxy.run().await
}
