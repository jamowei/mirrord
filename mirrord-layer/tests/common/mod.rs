use std::{
    collections::HashMap,
    fmt::Debug,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
};

use actix_codec::Framed;
use fancy_regex::Regex;
use futures::{SinkExt, StreamExt};
use k8s_openapi::chrono::Utc;
use mirrord_protocol::{
    tcp::{DaemonTcp, LayerTcp, NewTcpConnection, TcpClose, TcpData},
    ClientMessage, DaemonCodec, DaemonMessage,
};
use rstest::fixture;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
};

pub struct TestProcess {
    pub child: Option<Child>,
    stderr: Arc<Mutex<String>>,
    stdout: Arc<Mutex<String>>,
    error_capture: Regex,
}

impl TestProcess {
    pub fn get_stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    pub fn assert_stderr_empty(&self) {
        assert!(self.stderr.lock().unwrap().is_empty());
    }

    pub fn assert_log_level(&self, stderr: bool, level: &str) {
        if stderr {
            assert!(!self.stderr.lock().unwrap().contains(level));
        } else {
            assert!(!self.stdout.lock().unwrap().contains(level));
        }
    }

    fn from_child(mut child: Child) -> TestProcess {
        let stderr_data = Arc::new(Mutex::new(String::new()));
        let stdout_data = Arc::new(Mutex::new(String::new()));
        let child_stderr = child.stderr.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();
        let stderr_data_reader = stderr_data.clone();
        let stdout_data_reader = stdout_data.clone();
        let pid = child.id().unwrap();

        tokio::spawn(async move {
            let mut reader = BufReader::new(child_stderr);
            let mut buf = [0; 1024];
            loop {
                let n = reader.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }

                let string = String::from_utf8_lossy(&buf[..n]);
                eprintln!("stderr {:?} {pid}: {}", Utc::now(), string);
                {
                    stderr_data_reader.lock().unwrap().push_str(&string);
                }
            }
        });
        tokio::spawn(async move {
            let mut reader = BufReader::new(child_stdout);
            let mut buf = [0; 1024];
            loop {
                let n = reader.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                let string = String::from_utf8_lossy(&buf[..n]);
                print!("stdout {:?} {pid}: {}", Utc::now(), string);
                {
                    stdout_data_reader.lock().unwrap().push_str(&string);
                }
            }
        });

        let error_capture = Regex::new(r"^.*ERROR[^\w_-]").unwrap();

        TestProcess {
            child: Some(child),
            stderr: stderr_data,
            stdout: stdout_data,
            error_capture,
        }
    }

    pub async fn start_process(
        executable: String,
        args: Vec<String>,
        env: HashMap<&str, &str>,
    ) -> TestProcess {
        let child = Command::new(executable)
            .args(args)
            .envs(env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        println!("Started application.");
        TestProcess::from_child(child)
    }

    pub fn assert_stdout_contains(&self, string: &str) {
        assert!(self.stdout.lock().unwrap().contains(string));
    }

    pub fn assert_no_error_in_stdout(&self) {
        assert!(!self
            .error_capture
            .is_match(&self.stdout.lock().unwrap())
            .unwrap());
    }

    pub fn assert_no_error_in_stderr(&self) {
        assert!(!self
            .error_capture
            .is_match(&self.stderr.lock().unwrap())
            .unwrap());
    }

    pub async fn wait_assert_success(&mut self) {
        let awaited_child = self.child.take();
        let output = awaited_child.unwrap().wait_with_output().await.unwrap();
        assert!(output.status.success());
    }

    pub async fn wait(&mut self) {
        self.child.take().unwrap().wait().await.unwrap();
    }
}

pub struct LayerConnection {
    pub codec: Framed<TcpStream, DaemonCodec>,
    num_connections: u64,
}

impl LayerConnection {
    /// Accept a connection from the libraries and verify the first message it is supposed to send
    /// to the agent - GetEnvVarsRequest. Send back a response.
    /// Return the codec of the accepted stream.
    async fn accept_library_connection(listener: &TcpListener) -> Framed<TcpStream, DaemonCodec> {
        let (stream, _) = listener.accept().await.unwrap();
        println!("Got connection from library.");
        let mut codec = Framed::new(stream, DaemonCodec::new());
        let msg = codec.next().await.unwrap().unwrap();
        println!("Got first message from library.");
        if let ClientMessage::GetEnvVarsRequest(request) = msg {
            assert!(request.env_vars_filter.is_empty());
            assert_eq!(request.env_vars_select.len(), 1);
            assert!(request.env_vars_select.contains("*"));
        } else {
            panic!("unexpected request {:?}", msg)
        }
        codec
            .send(DaemonMessage::GetEnvVarsResponse(Ok(HashMap::new())))
            .await
            .unwrap();
        codec
    }

    /// Accept the library's connection and verify initial ENV message and PortSubscribe message
    /// caused by the listen hook.
    /// Handle flask's 2 process behaviour.
    pub async fn get_initialized_connection(
        listener: &TcpListener,
        app_port: u16,
    ) -> LayerConnection {
        let mut codec = Self::accept_library_connection(listener).await;
        let msg = match codec.next().await {
            Some(option) => option.unwrap(),
            None => {
                // Python runs in 2 processes, only one of which is the application. The library is
                // loaded into both so the first connection will not contain the application and
                // so will not send any of the messages that are generated by the hooks that are
                // triggered by the app.
                // So accept the next connection which will be the one by the library that was
                // loaded to the python process that actually runs the application.
                codec = Self::accept_library_connection(&listener).await;
                codec.next().await.unwrap().unwrap()
            }
        };
        assert_eq!(msg, ClientMessage::Tcp(LayerTcp::PortSubscribe(app_port)));
        LayerConnection {
            codec,
            num_connections: 0,
        }
    }

    pub async fn is_ended(&mut self) -> bool {
        self.codec.next().await.is_none()
    }

    /// Send the layer a message telling it the target got a new incoming connection.
    /// There is no such actual connection, because there is no target, but the layer should start
    /// a mirror connection with the application.
    /// Return the id of the new connection.
    async fn send_new_connection(&mut self) -> u64 {
        let new_connection_id = self.num_connections;
        self.codec
            .send(DaemonMessage::Tcp(DaemonTcp::NewConnection(
                NewTcpConnection {
                    connection_id: new_connection_id,
                    address: "127.0.0.1".parse().unwrap(),
                    destination_port: "80".parse().unwrap(),
                    source_port: "31415".parse().unwrap(),
                },
            )))
            .await
            .unwrap();
        self.num_connections += 1;
        new_connection_id
    }

    async fn send_tcp_data(&mut self, message_data: &str, connection_id: u64) {
        self.codec
            .send(DaemonMessage::Tcp(DaemonTcp::Data(TcpData {
                connection_id,
                bytes: Vec::from(message_data),
            })))
            .await
            .unwrap();
    }

    /// Send the layer a message telling it the target got a new incoming connection.
    /// There is no such actual connection, because there is no target, but the layer should start
    /// a mirror connection with the application.
    /// Return the id of the new connection.
    async fn send_close(&mut self, connection_id: u64) {
        self.codec
            .send(DaemonMessage::Tcp(DaemonTcp::Close(TcpClose {
                connection_id,
            })))
            .await
            .unwrap();
    }

    /// Tell the layer there is a new incoming connection, then send data "from that connection".
    pub async fn send_connection_then_data(&mut self, message_data: &str) {
        let new_connection_id = self.send_new_connection().await;
        self.send_tcp_data(message_data, new_connection_id).await;
        self.send_close(new_connection_id).await;
    }
}

#[derive(Debug)]
pub enum Application {
    Go19HTTP,
    NodeHTTP,
    PythonFastApiHTTP,
    PythonFlaskHTTP,
    PythonSelfConnect,
    PythonDontLoad,
    RustFileOps,
}

impl Application {
    /// Run python with shell resolving to find the actual executable.
    ///
    /// This is to help tests that run python with mirrord work locally on systems with pyenv.
    /// If we run `python3` on a system with pyenv the first executed is not python but bash. On mac
    /// that prevents the layer from loading because of SIP.
    async fn get_python3_executable() -> String {
        let mut python = Command::new("python3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let child_stdin = python.stdin.as_mut().unwrap();
        child_stdin
            .write_all(b"import sys\nprint(sys.executable)")
            .await
            .unwrap();
        let output = python.wait_with_output().await.unwrap();
        String::from(String::from_utf8_lossy(&output.stdout).trim())
    }

    pub async fn get_executable(&self) -> String {
        match self {
            Application::PythonFlaskHTTP
            | Application::PythonSelfConnect
            | Application::PythonDontLoad => Self::get_python3_executable().await,
            Application::PythonFastApiHTTP => String::from("uvicorn"),
            Application::NodeHTTP => String::from("node"),
            Application::Go19HTTP => String::from("tests/apps/app_go/19"),
            Application::RustFileOps => {
                format!(
                    "{}/{}",
                    env!("CARGO_MANIFEST_DIR"),
                    "../target/debug/fileops"
                )
            }
        }
    }

    pub fn get_args(&self) -> Vec<String> {
        let mut app_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        app_path.push("tests/apps/");
        match self {
            Application::PythonFlaskHTTP => {
                app_path.push("app_flask.py");
                println!("using flask server from {:?}", app_path);
                vec![String::from("-u"), app_path.to_string_lossy().to_string()]
            }
            Application::PythonDontLoad => {
                app_path.push("dont_load.py");
                println!("using script from {:?}", app_path);
                vec![String::from("-u"), app_path.to_string_lossy().to_string()]
            }
            Application::PythonFastApiHTTP => vec![
                String::from("--port=80"),
                String::from("--host=0.0.0.0"),
                String::from("--app-dir=tests/apps/"),
                String::from("app_fastapi:app"),
            ],
            Application::NodeHTTP => {
                app_path.push("app_node.js");
                vec![app_path.to_string_lossy().to_string()]
            }
            Application::Go19HTTP => vec![],
            Application::PythonSelfConnect => {
                app_path.push("self_connect.py");
                vec![String::from("-u"), app_path.to_string_lossy().to_string()]
            }
            Application::RustFileOps => vec![],
        }
    }

    pub fn get_app_port(&self) -> u16 {
        match self {
            Application::Go19HTTP
            | Application::NodeHTTP
            | Application::PythonFastApiHTTP
            | Application::PythonFlaskHTTP => 80,
            Application::PythonDontLoad | Application::RustFileOps => {
                unimplemented!("shouldn't get here")
            }
            Application::PythonSelfConnect => 1337,
        }
    }
}

/// Return the path to the existing layer lib, or build it first and return the path, according to
/// whether the environment variable MIRRORD_TEST_USE_EXISTING_LIB is set.
/// When testing locally the lib should be rebuilt on each run so that when developers make changes
/// they don't have to also manually build the lib before running the tests.
/// Building is slow on the CI though, so the CI can set the env var and use an artifact of an
/// earlier job on the same run (there are no code changes in between).
#[fixture]
#[once]
pub fn dylib_path() -> PathBuf {
    match std::env::var("MIRRORD_TEST_USE_EXISTING_LIB") {
        Ok(path) => {
            let dylib_path = PathBuf::from(path);
            println!("Using existing layer lib from: {:?}", dylib_path);
            assert!(dylib_path.exists());
            dylib_path
        }
        Err(_) => {
            let dylib_path = test_cdylib::build_current_project();
            println!("Built library at {:?}", dylib_path);
            dylib_path
        }
    }
}

pub fn get_env<'a>(dylib_path_str: &'a str, addr: &'a str) -> HashMap<&'a str, &'a str> {
    let mut env = HashMap::new();
    env.insert("RUST_LOG", "warn,mirrord=trace");
    env.insert("MIRRORD_IMPERSONATED_TARGET", "mock-target"); // Just pass some value.
    env.insert("MIRRORD_CONNECT_TCP", &addr);
    env.insert("MIRRORD_REMOTE_DNS", "false");
    env.insert("MIRRORD_FILE_OPS", "false");
    env.insert("DYLD_INSERT_LIBRARIES", dylib_path_str);
    env.insert("LD_PRELOAD", dylib_path_str);
    env
}
