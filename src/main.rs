use axum::{
    body:: {Body},
    extract::{Host, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{sse::{Event, KeepAlive, Sse}, Html, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use nanoid::nanoid;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, process::Stdio, sync::Arc};
use tokio::fs;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration, Instant};
use tokio_stream::StreamExt;

// --- 表定义 ---
const STREAMS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("streams");
const AUTH_TABLE: TableDefinition<&str, &str> = TableDefinition::new("auth");

// --- 数据结构 ---

#[derive(Serialize, Deserialize, Clone, Default)]
struct StreamMetadata {
    resolution: String,
    video_codec: String,
    audio_codec: String,
    channels: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct StreamConfig {
    #[serde(default)]
    id: String,
    name: String,
    url: String,
    #[serde(default)]
    proxy: String,
    #[serde(default)]
    metadata: StreamMetadata,
}

#[derive(Serialize, Clone)]
struct StreamResponse {
    id: String,
    name: String,
    url: String,
    m3u8_url: String,
    status: String,
    metadata: StreamMetadata,
}

struct AppState {
    processes: Mutex<HashMap<String, (String, String, Option<Child>, Instant, StreamMetadata, String)>>,
    db: Database,
    hls_base_dir: String,
}

#[tokio::main]
async fn main() {
    let base_shm = if std::path::Path::new("/dev/shm").exists() {
        "/dev/shm".to_string()
    } else {
        "./temp_streams".to_string()
    };

    let hls_dir = format!("{}/stream_hub", base_shm);

    if std::path::Path::new(&hls_dir).exists() {
        let _ = std::fs::remove_dir_all(&hls_dir);
        println!("🧹 已清理旧的缓存目录: {}", hls_dir);
    }
    
    std::fs::create_dir_all(&hls_dir).expect("无法创建缓存目录");

    let db = Database::builder()
        .create("streams_config.redb")
        .expect("无法创建数据库文件");

    let write_txn = db.begin_write().unwrap();
    {
        let _ = write_txn.open_table(STREAMS_TABLE).unwrap();
        let _ = write_txn.open_table(AUTH_TABLE).unwrap();
    }
    write_txn.commit().unwrap();

    let shared_state = Arc::new(AppState {
        processes: Mutex::new(HashMap::new()),
        db,
        hls_base_dir: hls_dir.clone(),
    });

    load_configs_to_memory(shared_state.clone()).await;

    let cleanup_state = shared_state.clone();
    tokio::spawn(async move {
        idle_cleanup_loop(cleanup_state).await;
    });

    let app = Router::new()
        .route("/", get(ui_handler))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/init", post(init_admin))
        .route("/api/auth/login", post(login_handler))
        .route("/api/auth/logout", post(logout_handler))
        .route("/api/streams", post(add_handler).get(list_handler)) // 合并路由写法
        .route("/api/streams/events", get(sse_handler)) 
        .route("/api/streams/:id", delete(stop_handler))
        .route("/api/streams/:id/start", post(start_stream_handler))
        .route("/api/streams/:id/stop", post(stop_stream_handler))
        .route("/api/playlist.m3u", get(playlist_handler))
        .route("/live/:id/*file", get(stream_handler))
        .with_state(shared_state);

    let port = 3000;
    let addr = format!("0.0.0.0:{}", port);
    
    // 获取本地内网 IP
    let local_ip = get_local_ip();

    println!("+-------------------------------------------------------+");
    println!("| 🚀 StreamHub (Native Auth) 已启动                  |");
    println!("+-------------------------------------------------------+");
    println!("| > 本地访问: http://localhost:{}                  |", port);
    println!("| > 内网访问: http://{}:{}             |", local_ip, port);
    println!("+-------------------------------------------------------+");
    println!("| 监控目录: {} ", hls_dir); 
    println!("+-------------------------------------------------------+");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn get_local_ip() -> String {
    // 技巧：连接到一个公共地址（如 DNS 服务），
    // 操作系统会选择最合适的局域网网卡 IP。
    // 这里不会产生实际的网络通信。
    let socket = std::net::UdpSocket::bind("0.0.0.0:0");
    match socket {
        Ok(s) => {
            if s.connect("8.8.8.8:80").is_ok() {
                if let Ok(addr) = s.local_addr() {
                    return addr.ip().to_string();
                }
            }
            "127.0.0.1".to_string()
        }
        Err(_) => "127.0.0.1".to_string(),
    }
}

// --- 认证逻辑辅助函数 (只靠 HeaderMap) ---

fn is_authenticated(headers: &HeaderMap) -> bool {
    headers.get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(|s| s.contains("login_session=true"))
        .unwrap_or(false)
}

// --- 身份验证 Handlers ---

async fn auth_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let read_txn = state.db.begin_read().unwrap();
    let table = read_txn.open_table(AUTH_TABLE).unwrap();
    let has_admin = table.iter().unwrap().count() > 0;
    Json(serde_json::json!({ "initialized": has_admin }))
}

async fn init_admin(State(state): State<Arc<AppState>>, Json(payload): Json<serde_json::Value>) -> StatusCode {
    let write_txn = state.db.begin_write().unwrap();
    {
        let mut table = write_txn.open_table(AUTH_TABLE).unwrap();
        if table.iter().unwrap().count() > 0 { return StatusCode::FORBIDDEN; }
        
        let user = payload["username"].as_str().unwrap_or("admin");
        let pass = payload["password"].as_str().unwrap_or("");
        table.insert(user, pass).unwrap();
    }
    write_txn.commit().unwrap();
    StatusCode::OK
}

async fn login_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<serde_json::Value>
) -> impl IntoResponse {
    let user = payload["username"].as_str().unwrap_or("");
    let pass = payload["password"].as_str().unwrap_or("");

    let read_txn = state.db.begin_read().unwrap();
    let table = read_txn.open_table(AUTH_TABLE).unwrap();
    
    if let Ok(Some(stored_pass)) = table.get(user) {
        if stored_pass.value() == pass {
            return (
                StatusCode::OK,
                [(header::SET_COOKIE, "login_session=true; Path=/; HttpOnly; SameSite=Lax")],
                "Success"
            ).into_response();
        }
    }
    (StatusCode::UNAUTHORIZED, "Fail").into_response()
}

async fn logout_handler() -> impl IntoResponse {
    // 使用 builder 模式可以更优雅地设置 Header 和 Body
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::SET_COOKIE,
            "login_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
        )
        .body(Body::from("Logged out"))
        .unwrap_or_else(|_| {
            // 如果构建失败，返回简单的错误响应
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}

// --- 业务 Handlers ---

async fn sse_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>
) -> impl IntoResponse {
    if !is_authenticated(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let stream = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(Duration::from_secs(2)))
        .map(move |_| {
            let procs = state.processes.try_lock();
            let list: Vec<StreamResponse> = if let Ok(p) = procs {
                p.iter().map(|(id, (name, url, child, _, metadata, _))| StreamResponse {
                    id: id.clone(),
                    name: name.clone(),
                    url: url.clone(),
                    m3u8_url: format!("/live/{}/index.m3u8", id),
                    status: if child.is_some() { "Running" } else { "Standby" }.to_string(),
                    metadata: metadata.clone(),
                }).collect()
            } else { vec![] };
            let data = serde_json::to_string(&list).unwrap_or_default();
            Ok::<Event, std::convert::Infallible>(Event::default().data(data))
        });
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

async fn add_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>, 
    Json(mut payload): Json<StreamConfig>
) -> impl IntoResponse {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let alphabet: [char; 26] = [
        'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 
        'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z'
    ];

    let id = nanoid!(6, &alphabet);
    
    payload.id = id.clone();
    let metadata = get_stream_metadata(&payload.url, &payload.proxy).await;
    payload.metadata = metadata.clone();

    let write_txn = state.db.begin_write().unwrap();
    {
        let mut table = write_txn.open_table(STREAMS_TABLE).unwrap();
        // 因为 id 现在是短的，直接用 id.as_str() 即可
        table.insert(id.as_str(), serde_json::to_string(&payload).unwrap().as_str()).unwrap();
    }
    write_txn.commit().unwrap();

    state.processes.lock().await.insert(
        id.clone(), 
        (payload.name.clone(), payload.url.clone(), None, Instant::now() - Duration::from_secs(120), metadata.clone(), payload.proxy.clone())
    );

    // 返回短 ID 给前端
    Json(StreamResponse { 
        id: id.clone(), 
        name: payload.name, 
        url: payload.url, 
        m3u8_url: format!("/live/{}/index.m3u8", id), 
        status: "Standby".to_string(), 
        metadata 
    }).into_response()
}

async fn list_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>
) -> impl IntoResponse {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let procs = state.processes.lock().await;
    let list: Vec<StreamResponse> = procs.iter().map(|(id, (name, url, child, _, metadata, _))| StreamResponse {
        id: id.clone(), name: name.clone(), url: url.clone(),
        m3u8_url: format!("/live/{}/index.m3u8", id),
        status: if child.is_some() { "Running" } else { "Standby" }.to_string(),
        metadata: metadata.clone(),
    }).collect();
    Json(list).into_response()
}

async fn start_stream_handler(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>
) -> impl IntoResponse {
    if !is_authenticated(&headers) { return StatusCode::UNAUTHORIZED.into_response(); }
    
    // 1. 先尝试获取必要的参数，如果流已经在运行则直接返回
    let config = {
        let procs = state.processes.lock().await;
        if let Some((_, url, child_opt, _, _, p_url)) = procs.get(&id) {
            if child_opt.is_some() { 
                return (StatusCode::OK, "Already Running").into_response(); 
            }
            // 只克隆我们需要的数据
            Some((url.clone(), p_url.clone()))
        } else {
            None
        }
    };

    // 2. 根据获取到的配置启动 FFMPEG
    if let Some((url, proxy)) = config {
        let child = spawn_ffmpeg(&url, &id, &state.hls_base_dir, &proxy).await;
        let mut procs = state.processes.lock().await;
        if let Some((_, _, child_opt, last_access, _, _)) = procs.get_mut(&id) {
            *child_opt = Some(child);
            *last_access = Instant::now();
            (StatusCode::OK, "Started").into_response()
        } else {
            StatusCode::NOT_FOUND.into_response()
        }
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn stop_stream_handler(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>
) -> impl IntoResponse {
    if !is_authenticated(&headers) { return StatusCode::UNAUTHORIZED.into_response(); }
    
    let mut procs = state.processes.lock().await;
    if let Some((_, _, child_opt, _, _, _)) = procs.get_mut(&id) {
        if let Some(mut child) = child_opt.take() {
            let _ = child.kill().await;
            // 清理临时文件
            let _ = tokio::fs::remove_dir_all(format!("{}/{}", state.hls_base_dir, id)).await;
            return (StatusCode::OK, "Stopped").into_response();
        }
    }
    (StatusCode::OK, "Not Running").into_response()
}

async fn stop_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>, 
    Path(id): Path<String>
) -> impl IntoResponse {
    if !is_authenticated(&headers) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let write_txn = state.db.begin_write().unwrap();
    { let mut table = write_txn.open_table(STREAMS_TABLE).unwrap(); table.remove(id.as_str()).unwrap(); }
    write_txn.commit().unwrap();
    let mut procs = state.processes.lock().await;
    if let Some((_, _, child_opt, _, _, _)) = procs.remove(&id) { if let Some(mut child) = child_opt { let _ = child.kill().await; } }
    Json(serde_json::json!({"status": "deleted"})).into_response()
}

// --- 辅助逻辑 (保持不变) ---

async fn load_configs_to_memory(state: Arc<AppState>) {
    let read_txn = state.db.begin_read().unwrap();
    let table = read_txn.open_table(STREAMS_TABLE).unwrap();
    let mut procs = state.processes.lock().await;
    for item in table.iter().unwrap() {
        if let Ok((_, val)) = item {
            if let Ok(cfg) = serde_json::from_str::<StreamConfig>(val.value()) {
                procs.insert(
                    cfg.id.clone(),
                    (cfg.name, cfg.url, None, Instant::now() - Duration::from_secs(120), cfg.metadata, cfg.proxy),
                );
            }
        }
    }
}

async fn get_stream_metadata(url: &str, proxy: &str) -> StreamMetadata {
    let mut meta = StreamMetadata {
        resolution: "检测失败".to_string(),
        video_codec: "未知".to_string(),
        audio_codec: "未知".to_string(),
        channels: "未知".to_string(),
    };
    let mut args = vec![
        "-v", "quiet", "-print_format", "json", "-show_streams",
        "-user_agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) PotPlayer/23.0.0",
        "-protocol_whitelist", "file,rtp,udp,tcp,http,https,tls,crypto,socks",
        "-probesize", "10M", "-analyzeduration", "5000000",
    ];
    args.push(url);
    let mut cmd = Command::new("ffprobe");
    cmd.args(&args);
    if !proxy.is_empty() { cmd.env("all_proxy", proxy); }
    let output = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await;
    if let Ok(Ok(out)) = output {
        let stdout_str = String::from_utf8_lossy(&out.stdout);
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout_str) {
            if let Some(streams) = json["streams"].as_array() {
                for stream in streams {
                    match stream["codec_type"].as_str().unwrap_or("") {
                        "video" => {
                            let (w, h) = (stream["width"].as_i64().unwrap_or(0), stream["height"].as_i64().unwrap_or(0));
                            if w > 0 {
                                meta.resolution = format!("{}p ({}x{})", h, w, h);
                                meta.video_codec = stream["codec_name"].as_str().unwrap_or("").to_uppercase();
                            }
                        }
                        "audio" => {
                            meta.audio_codec = stream["codec_name"].as_str().unwrap_or("").to_uppercase();
                            meta.channels = format!("{} 声道", stream["channels"].as_i64().unwrap_or(0));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    meta
}

async fn spawn_ffmpeg(url: &str, id: &str, base_dir: &str, proxy: &str) -> Child {
    let output_dir = format!("{}/{}", base_dir, id);
    let _ = std::fs::create_dir_all(&output_dir).unwrap();
    
    let url_lower = url.to_lowercase();

    // 自动提取 Referer (解决 107.150... 这种源的 404/403 问题)
    let referer = if let Some(pos) = url.find("://") {
        let rest = &url[pos + 3..];
        if let Some(slash_pos) = rest.find('/') {
            &url[..pos + 3 + slash_pos + 1]
        } else { url }
    } else { "" };

    // 1. 基础参数 (通用伪装和协议白名单)
    let mut args = vec![
        "-loglevel".to_string(), "warning".to_string(),
        "-user_agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_string(),
        "-headers".to_string(), format!("Referer: {}\r\nAccept: */*\r\nConnection: keep-alive\r\n", referer),
        // 默认开启全协议白名单，防止嵌套协议加载失败
        "-protocol_whitelist".to_string(), "file,http,https,tcp,tls,rtp,udp,rtsp,crypto".to_string(),
    ];

    // 2. 输入适配优化
    args.extend([
        "-fflags".to_string(), "genpts+igndts+nobuffer".to_string(),
        "-flags".to_string(), "low_delay".to_string(),
        "-probesize".to_string(), "15M".to_string(), 
        "-analyzeduration".to_string(), "15M".to_string(),
    ]);

    // 3. 协议特定注入
    if url_lower.starts_with("rtsp://") {
        args.extend(["-rtsp_transport".to_string(), "tcp".to_string()]);
    } else if url_lower.contains(".m3u8") {
        args.extend(["-allowed_extensions".to_string(), "ALL".to_string()]);
    } else if url_lower.contains("/rtp/") {
        // 移除报错的 fifo_size，改用更通用的 udp 缓冲区参数
        args.extend(["-buffer_size".to_string(), "1024000".to_string()]);
    }

    // 4. 注入输入源
    args.extend(["-i".to_string(), url.to_string()]);

    // 5. 统一输出层 (HLS)
    args.extend([
        "-c".to_string(), "copy".to_string(),
        "-f".to_string(), "hls".to_string(),
        "-hls_time".to_string(), "4".to_string(),
        "-hls_list_size".to_string(), "6".to_string(),
        "-hls_flags".to_string(), "delete_segments+append_list+independent_segments".to_string(),
        "-hls_segment_type".to_string(), "mpegts".to_string(),
        "-hls_segment_filename".to_string(), format!("{}/seg_%d.ts", output_dir), // 显式命名切片
        format!("{}/index.m3u8", output_dir),
    ]);

    let mut cmd = Command::new("ffmpeg");
    cmd.args(&args)
       .stdin(Stdio::null())
    //    .stdout(Stdio::piped())
       .stdout(Stdio::null())
       .stderr(Stdio::inherit())
       .kill_on_drop(true);

    // 只有非内网且是非 RTP 地址时应用代理
    if !proxy.is_empty() && !url_lower.contains("172.16.") && !url_lower.starts_with("rtp://") {
        cmd.env("all_proxy", proxy).env("http_proxy", proxy).env("https_proxy", proxy);
    }

    match cmd.spawn() {
        Ok(child) => {
            println!("✅ FFmpeg 进程已派生，PID: {:?}", child.id());
            child
        }
        Err(e) => {
            eprintln!("🔥 无法启动 FFmpeg 命令: {}", e);
            panic!("FFmpeg 启动失败，请检查是否已安装并加入 PATH");
        }
    }
}

async fn stream_handler(Path((id, file)): Path<(String, String)>, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let is_m3u8 = file.ends_with(".m3u8");
    let (mut need_spawn, mut stream_url, mut proxy) = (false, String::new(), String::new());
    {
        let mut procs = state.processes.lock().await;
        if let Some((_, url, child_opt, last_access, _, p_url)) = procs.get_mut(&id) {
            *last_access = Instant::now();
            if is_m3u8 && child_opt.is_none() { 
                need_spawn = true; 
                stream_url = url.clone();
                proxy = p_url.clone();
            }
        }
    }
    if need_spawn {
        let child = spawn_ffmpeg(&stream_url, &id, &state.hls_base_dir, &proxy).await;
        let mut procs = state.processes.lock().await;
        if let Some((_, _, child_opt, _, _, _)) = procs.get_mut(&id) { *child_opt = Some(child); }
        let m3u8_path = std::path::Path::new(&state.hls_base_dir).join(&id).join("index.m3u8");
        for _ in 0..15 { // 最多等 3s
             if m3u8_path.exists() { break; }
             sleep(Duration::from_millis(200)).await;
        }
    }
    let file_path = std::path::Path::new(&state.hls_base_dir).join(&id).join(&file);
    match tokio::fs::read(&file_path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, if file.ends_with(".m3u8") { "application/x-mpegurl" } else { "video/MP2T" })], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Loading...").into_response(),
    }
}

async fn playlist_handler(State(state): State<Arc<AppState>>, Host(host): Host) -> impl IntoResponse {
    let read_txn = state.db.begin_read().unwrap();
    let table = read_txn.open_table(STREAMS_TABLE).unwrap();
    let mut m3u = String::from("#EXTM3U\n");
    for item in table.iter().unwrap() {
        if let Ok((_, val)) = item {
            let cfg: StreamConfig = serde_json::from_str(val.value()).unwrap();
            m3u.push_str(&format!("#EXTINF:-1 tvg-id=\"{}\",{}\nhttp://{}/live/{}/index.m3u8\n", cfg.id, cfg.name, host, cfg.id));
        }
    }
    ([(header::CONTENT_TYPE, "application/x-mpegurl")], m3u)
}

async fn idle_cleanup_loop(state: Arc<AppState>) {
    loop {
        sleep(Duration::from_secs(10)).await;
        let mut procs = state.processes.lock().await;
        for (id, (_name, _, child_opt, last_access, _, _)) in procs.iter_mut() {
            if child_opt.is_some() && last_access.elapsed() > Duration::from_secs(60) {
                if let Some(mut child) = child_opt.take() { let _ = child.kill().await; }
                let _ = tokio::fs::remove_dir_all(format!("{}/{}", state.hls_base_dir, id)).await;
            }
        }
    }
}

async fn ui_handler() -> impl IntoResponse {
    match fs::read_to_string("index.html").await {
        Ok(html) => Html(html).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "index.html missing").into_response(),
    }
}