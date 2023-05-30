use futures_util::{
    SinkExt,
    stream::{
        StreamExt as FuturesStreamExt,
        SplitStream,
        SplitSink
    }
};

use systemstat::{System as statSystem, Platform as statPlatform, saturating_sub_bytes};

use serde::Serialize;
use serde_json::{json, Value, to_string};
use std::{sync::{
    Arc,
    atomic::{
        AtomicBool,
        Ordering
    }
}, str::FromStr};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use std::env;
use std::path::Path;

use sysinfo::{
    CpuExt,
    NetworkExt,
    NetworksExt,
    ProcessExt,
    System,
    SystemExt
};

use tokio::{
    net::{
        TcpListener,
        TcpStream
    },
    sync::Mutex as TokioMutex,
    time::timeout, io::AsyncWriteExt
};

use tokio_tungstenite::{
    accept_async,
    WebSocketStream, tungstenite::Message
};

use std::time::Instant;

macro_rules! time_lock_acquisition {
    ($name:ident, $lock:expr, $threshold:expr) => {
        {
            let start_time = Instant::now();
            let lock = $lock.lock().await;
            let duration = start_time.elapsed();
            if cfg!(debug_assertions) && duration >= $threshold {
                println!("{} lock acquisition time: {:?}", stringify!($name), duration);
            }
            lock
        }
    };
}

macro_rules! check_stopped {
    ($app_state:expr) => {
        if $app_state.isStopped() {
            break;
        }
    };
}

#[macro_export]
macro_rules! async_listener {
    ($key:expr, $app:expr) => {{
        use crossterm::{
            event::{poll, read, Event, KeyCode},
        };
        use tokio::task::yield_now;

        // Create a future that waits for the key combination
        let key_future = async move {
            loop {
                yield_now().await;

                if poll(std::time::Duration::from_millis(25)).expect("Failed to poll for events") {
                    if let Event::Key(key_event) = read().expect("Failed to read event") {
                        if key_event.code == KeyCode::Char($key.chars().next().unwrap()) {
                            $app.stop();
                            break;
                        }
                    }
                }
            }
        };

        // Return the key combination future
        key_future
    }};
}





#[derive(Serialize, Clone, Debug)]
struct CPU {
    name: String,
    cpu_usage: f32,
    vendor_id: String,
    brand: String,
    frequency: u64
}
#[derive(Serialize, Clone, Debug)]
struct NetworkInterface {
    name: String,
    received: u64,
    send: u64
}
#[derive(Serialize, Clone, Debug)]
struct SystemMemoryInfo {
    total_capacity: u64,
    used: u64,
    swap_capacity: u64,
    swap_used: u64
}
#[derive(Serialize, Clone, Debug)]
struct SystemInfo {
    Name: String,
    Hostname: String,
    Kernel_Version: String,
    OS_Version: String
}
#[derive(Serialize, Clone, Debug)]
struct SInfoJson {
    CPUs: Vec<CPU>,
    Memory: SystemMemoryInfo,
    Network: Vec<NetworkInterface>,
    System: SystemInfo
}

impl SInfoJson {
    pub fn new(sys: &System) -> Self {
        let mut cpus = Vec::new();
        /*
        let cpu_info = CPU {
                name: cpu,
                cpu_usage: cpu.get_cpu_usage(),
                vendor_id: cpu.vendor_id().to_owned(),
                brand: cpu.brand().to_owned(),
                frequency: cpu.frequency()
            };
         */
        let _cpus = sys.cpus();
        _cpus.iter().for_each(|cpu: &sysinfo::Cpu| {
            let _CPU = CPU {
                name: cpu.name().to_owned().trim().to_owned().trim_end().to_owned(),
                cpu_usage: cpu.cpu_usage(),
                vendor_id: cpu.vendor_id().to_owned(),
                brand: cpu.brand().to_owned().trim().to_owned(),
                frequency: cpu.frequency(),
            };
            cpus.push(_CPU);
        });
        let mut network_interfaces = Vec::new();

        for (name, data) in sys.networks() {
            let network_interface = NetworkInterface {
                name: name.to_owned(),
                received: data.received(),
                send: data.transmitted()
            };
            network_interfaces.push(network_interface);
        }

        let memory_info = SystemMemoryInfo {
            total_capacity: sys.total_memory(),
            used: sys.used_memory(),
            swap_capacity: sys.total_swap(),
            swap_used: sys.used_swap()
        };
        let sys_inf = SystemInfo {
            Name: sys.name().unwrap_or("unknown".to_owned()),
            Hostname: sys.host_name().unwrap_or("unknown".to_owned()),
            Kernel_Version: sys.kernel_version().unwrap_or("unknown".to_owned()),
            OS_Version: sys.os_version().unwrap_or("unknown".to_owned())
        };
        SInfoJson {
            CPUs: cpus,
            Memory: memory_info,
            Network: network_interfaces,
            System: sys_inf
        }
    }

    pub fn update(&mut self, sys: &System) {
        self.CPUs.clear();
        let cpus = sys.cpus();
        cpus.iter().for_each(|cpu| {
            let _CPU = CPU {
                name: cpu.name().trim().to_owned(),
                cpu_usage: cpu.cpu_usage(),
                vendor_id: cpu.vendor_id().to_owned(),
                brand: cpu.brand().trim().to_owned(),
                frequency: cpu.frequency(),
            };
            self.CPUs.push(_CPU);
        });

        self.Network.clear();
        for (name, data) in sys.networks() {
            let network_interface = NetworkInterface {
                name: name.to_owned(),
                received: data.received(),
                send: data.transmitted(),
            };
            self.Network.push(network_interface);
        }

        self.Memory.total_capacity = sys.total_memory();
        self.Memory.used = sys.used_memory();
        self.Memory.swap_capacity = sys.total_swap();
        self.Memory.swap_used = sys.used_swap();

        self.System.Name = sys.name().unwrap_or_else(|| "unknown".to_owned());
        self.System.Hostname = sys.host_name().unwrap_or_else(|| "unknown".to_owned());
        self.System.Kernel_Version = sys.kernel_version().unwrap_or_else(|| "unknown".to_owned());
        self.System.OS_Version = sys.os_version().unwrap_or_else(|| "unknown".to_owned());
    }

    pub fn default() -> Self {
        let sys = System::new_all();
        SInfoJson::new(&sys)
    }

    fn to_json(&self) -> String {
        serde_json::to_string(&self).unwrap()
    }
}



//define app state as a struct, this will store all the information we need to have persist across tasks
#[derive(Clone)]
struct AppState {
    sysinfo_instance: Arc<TokioMutex<SInfoJson>>,
    atomic_flag: Arc<AtomicBool>,
    clients: Arc<TokioMutex<Vec<SplitPipeWebSocket>>>
}

impl AppState {
    pub fn new() -> Self {
        let sysinfo_instance = Arc::new(TokioMutex::new(SInfoJson::default()));
        let atomic_flag = Arc::new(AtomicBool::new(false));
        let cList = Arc::new(TokioMutex::new(Vec::<SplitPipeWebSocket>::new()));
        AppState {
            sysinfo_instance,
            atomic_flag,
            clients: cList
        }
    }
    pub fn stop(&mut self) {
        self.atomic_flag.store(true, Ordering::SeqCst);
    }
    pub fn isStopped(&self) -> bool {
        self.atomic_flag.load(Ordering::SeqCst)
    }
}

async fn poll_system(app: AppState) {
    let mut system = System::new_all();
    loop {
        check_stopped!(app); //will break the loop if app.stop was called, this makes it rather easy to make all the threads stop
        {
            system.refresh_all();
            let mut sys = time_lock_acquisition!(SystemPoll, app.sysinfo_instance, std::time::Duration::from_micros(250));
            sys.update(&system);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

async fn handle_connections(app: AppState, listener: TcpListener) {
    {
        loop {
            check_stopped!(app);
    
            // Set the maximum duration for waiting for a connection
            let timeout_duration = std::time::Duration::from_secs_f64(2.);
    
            // Wait for a connection with a timeout
            match timeout(timeout_duration, listener.accept()).await {
                Ok(Ok((stream, _))) => {
                    if let Ok(ws_stream) = accept_async(stream).await {
                        tokio::spawn(handle_websocket(ws_stream, app.clone()));
                    }
                }
                Ok(Err(err)) => {
                    // Handle the error as needed
                }
                Err(_) => {
                    // Timeout occurred, we don't really need to do anything as doing this is mostly for allowing graceful termination, rather than being a real issue
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let config = find_or_generate_config().await.expect("Error loading config.");
    let mut _global_state = AppState::new();
    let global_state_C = _global_state.clone(); //since we are using move into an async function, and I do not want _global_state to move, we create a copy here that will move into the closure
    let ip = match config["ServerIP"].as_str() {
        Some(val) => {
            val
        }
        None => {
            println!("The config did not contain value ServerIP, listening to all interfaces instead!");
            "0.0.0.0"
        }
    };
    let port = config["port"].as_f64().unwrap_or(3000.);
    let addr = format!("{}:{}", ip, port.floor() as u64);
    let listener = TcpListener::bind(&addr).await.expect("Failed to bind address");
    let mut handles: Vec<tokio::task::JoinHandle<()>> = vec![
        tokio::spawn(poll_system(_global_state.clone())),
        tokio::spawn(handle_connections(_global_state.clone(), listener)),
        tokio::spawn(send_updates_to_all(_global_state.clone()))
    ];
    let mut global_state_C = _global_state.clone();
    println!("Server started and listening on {}", addr.replace("0.0.0.0", "*"));
    tokio::spawn(async_listener!("t", global_state_C)).await;
    //join all handles
    println!("All tasks are attempting to spool down... application will stop soon after...");
    for (index, handle) in handles.iter_mut().enumerate() {
        let val: Result<Result<(), tokio::task::JoinError>, tokio::time::error::Elapsed> = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        match val {
            Ok(_) => {
                println!("Task ID: {} has stopped without issue!", index);
            }
            Err(_) => {
                println!("Task ID: {} has been forcefully terminated, it took too long to stop.", {index});
            }
        }
    }
    println!("All tasks are stopped, terminating.....");
}
#[derive(Debug)]
struct SplitPipeWebSocket {
    write: SplitSink<WebSocketStream<TcpStream>, Message>
}
impl SplitPipeWebSocket {
    pub async fn send(&mut self, msg: String) {
        self.write.send(Message::Text(msg)).await;
    }
}
async fn handle_websocket(ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, app: AppState) {
    if (app.isStopped()) {
        println!("A socket tried to connect during spindown, Ignoring!");
        return;
    }
    // Handle incoming WebSocket messages and events here

    // Example: Echo back received messages
    let (mut write, mut read) = ws_stream.split();
    let mut clientList = time_lock_acquisition!(HandleSocketGetClients, app.clients, std::time::Duration::from_micros(250));
    clientList.push(SplitPipeWebSocket { write: write});
    drop(clientList);
    loop {
        check_stopped!(app);
        match tokio::time::timeout(std::time::Duration::from_millis(100), read.next()).await {
            Ok(Some(Ok(msg))) => {
                match(msg) {
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            Ok(Some(Err(err))) => {
                // Handle the error as needed
            }
            Ok(None) => {
                // No more messages, the connection was closed
                break;
            }
            Err(_) => {
                // Timeout occurred, we don't really need to do anything as doing this is mostly for allowing graceful termination, rather than being a real issue
            }
        }
    }
}
async fn send_updates_to_all(app: AppState) {
    loop {
        check_stopped!(app);
        let mut clients = time_lock_acquisition!(UpdateGetClients, app.clients, std::time::Duration::from_micros(250));
        let sys = time_lock_acquisition!(UpdateGetSystem, app.sysinfo_instance, std::time::Duration::from_micros(250));
        let json = sys.to_json();
        drop(sys);
        let json = serde_json::to_string(&json).unwrap_or("{\"Error\": \"Could not parse struct to json\"}".to_owned());
        for (index, socket) in clients.iter_mut().enumerate() {
            tokio::time::timeout(std::time::Duration::from_secs_f64(1.0 / 10.), socket.send(json.clone())).await;
        }
        drop(clients);
        std::thread::sleep(std::time::Duration::from_secs_f64(0.20));
    }
}
async fn find_or_generate_config() -> Option<Value> {
    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let path = exe_dir.canonicalize().unwrap_or(std::path::PathBuf::from_str("C:/").unwrap());
            let config_path_str = &(exe_dir.to_str().unwrap().to_owned() + "\\config.json");
            let config_path = Path::new(config_path_str);

            if !config_path.exists() {
                match tokio::fs::File::create(config_path).await {
                    Ok(mut f) => {
                        let json = json!({
                            "ServerIP": "0.0.0.0",
                            "port": 3000
                        });
                        let json_bytes = serde_json::to_string_pretty(&json).unwrap();
                        let json_bytes = json_bytes.as_bytes();
                        if let Err(_) = f.write_all(&json_bytes).await {
                            println!("Config not found, generating one... Error!");
                            return None;
                        }
                        println!("Config not found, generating one... Success!");
                    },
                    Err(_) => {
                        println!("Config not found, generating one... Error!");
                        return None;
                    }
                }
            }

            let mut file = match tokio::fs::File::open(config_path).await {
                Ok(file) => file,
                Err(_) => {
                    println!("Failed to open config file!");
                    return None;
                }
            };

            let mut contents = String::new();
            if let Err(_) = file.read_to_string(&mut contents).await {
                println!("Failed to read config file!");
                return None;
            }

            let config: Value = match serde_json::from_str(&contents) {
                Ok(config) => config,
                Err(_) => {
                    println!("Failed to parse config file as JSON!");
                    return None;
                }
            };

            Some(config)
        } else {
            None
        }
    } else {
        None
    }
}