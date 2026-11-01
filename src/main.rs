fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gijiroku=info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("admin") {
        if let Err(err) = gijiroku::admin(&args[1..]) {
            eprintln!("{err:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(err) = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(gijiroku::serve())
    {
        eprintln!("起動に失敗しました: {err:#}");
        std::process::exit(1);
    }
}
