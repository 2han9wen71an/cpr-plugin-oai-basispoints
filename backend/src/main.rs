use cpr_plugin_oai_basispoints::{PLUGIN_ID, manifest, plugin};
use gateway_plugin_sdk::client::{PluginSession, SessionConfig, SessionError};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    let manifest = manifest()?;
    if session.handshake().plugin_id != PLUGIN_ID
        || session.handshake().contributes != manifest.contributes
    {
        return Err(SessionError::Handshake.into());
    }
    let config = session.handshake().configuration.clone();
    let runtime = cpr_plugin_oai_basispoints::RuntimeConfig::from_configuration(&config)?;
    session.run(plugin(std::sync::Arc::new(runtime))?).await?;
    Ok(())
}
