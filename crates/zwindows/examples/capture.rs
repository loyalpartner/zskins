use std::time::Duration;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let result = zwindows::toplevel_capture::capture_toplevels(Duration::from_millis(2000));
    match result {
        Ok(map) => {
            println!("captured {} toplevels:", map.len());
            for ((app_id, title), buf) in &map {
                println!("  ({app_id}, {title}) => {}x{}", buf.width, buf.height);
            }
        }
        Err(e) => {
            eprintln!("capture failed: {e}");
        }
    }
}
