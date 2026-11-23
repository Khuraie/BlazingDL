//! blazingdl-mcp: BlazingDL's MCP server. An AI assistant starts it and
//! speaks MCP with it on stdin and stdout; it hands what the assistant asks to the running app
//! (docs/ai-assistants.md).

use rmcp::ServiceExt;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Started by hand in a terminal: say what it is instead of waiting silently.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("blazingdl-mcp {}", env!("CARGO_PKG_VERSION"))
            }
            _ => println!(
                "blazingdl-mcp: BlazingDL's MCP server, for AI \
                 assistants. The assistant starts it (no arguments) and talks to it on stdin and \
                 stdout. Setup: \
                 https://blazingdl.app/help/ai-assistants.html"
            ),
        }
        return Ok(());
    }
    let server = blazingdl_mcp::Server::new(std::sync::Arc::new(blazingdl_ipc::request));
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
