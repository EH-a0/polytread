#[cfg(windows)]
mod platform {
    use std::io;
    use std::time::Duration;

    use anyhow::{Context, Result, bail};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    const PIPE_NAME: &str = r"\\.\pipe\xyz.polytread.cli.control.v1";
    const SHUTDOWN_REQUEST: &[u8] = b"POLYTREAD_SHUTDOWN_V1\n";
    const SHUTDOWN_ACCEPTED: &[u8] = b"POLYTREAD_SHUTDOWN_ACCEPTED_V1\n";
    const PIPE_IO_TIMEOUT: Duration = Duration::from_secs(2);

    pub fn spawn_shutdown_listener(
        shutdown_tx: mpsc::Sender<()>,
    ) -> Result<Option<JoinHandle<()>>> {
        let server = create_server(PIPE_NAME)
            .context("failed to open PolyTread's same-user shutdown channel")?;
        Ok(Some(tokio::spawn(async move {
            if let Err(error) = serve(server, shutdown_tx).await {
                tracing::warn!(%error, "same-user shutdown channel stopped");
            }
        })))
    }

    pub async fn request_shutdown() -> Result<bool> {
        request_shutdown_at(PIPE_NAME).await
    }

    fn create_server(name: &str) -> io::Result<NamedPipeServer> {
        // Windows' default pipe DACL grants write access to the creator owner,
        // administrators, and LocalSystem; Everyone receives read-only access. Requiring
        // a duplex client therefore keeps shutdown same-user, while the explicit remote
        // rejection prevents direct SMB pipe access.
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .create(name)
    }

    async fn serve(mut server: NamedPipeServer, shutdown_tx: mpsc::Sender<()>) -> Result<()> {
        loop {
            tokio::select! {
                result = server.connect() => {
                    result.context("failed accepting a local shutdown client")?;
                }
                _ = shutdown_tx.closed() => return Ok(()),
            }

            let accepted = tokio::time::timeout(PIPE_IO_TIMEOUT, async {
                let mut request = vec![0_u8; SHUTDOWN_REQUEST.len()];
                server.read_exact(&mut request).await?;
                if request != SHUTDOWN_REQUEST {
                    return Ok::<bool, io::Error>(false);
                }
                shutdown_tx
                    .send(())
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "service stopped"))?;
                server.write_all(SHUTDOWN_ACCEPTED).await?;
                server.flush().await?;
                Ok(true)
            })
            .await;

            match accepted {
                Ok(Ok(true)) => return Ok(()),
                Ok(Ok(false)) | Ok(Err(_)) | Err(_) => {
                    server
                        .disconnect()
                        .context("failed resetting the local shutdown channel")?;
                }
            }
        }
    }

    async fn request_shutdown_at(name: &str) -> Result<bool> {
        let mut client = match ClientOptions::new().read(true).write(true).open(name) {
            Ok(client) => client,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).context("failed connecting to PolyTread's shutdown channel");
            }
        };

        tokio::time::timeout(PIPE_IO_TIMEOUT, async {
            client.write_all(SHUTDOWN_REQUEST).await?;
            client.flush().await?;
            let mut accepted = vec![0_u8; SHUTDOWN_ACCEPTED.len()];
            client.read_exact(&mut accepted).await?;
            if accepted != SHUTDOWN_ACCEPTED {
                bail!("PolyTread's shutdown channel returned an invalid response");
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("PolyTread's shutdown channel did not respond in time")??;
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn same_user_pipe_delivers_an_authenticated_shutdown_request() {
            let pipe_name = format!(
                r"\\.\pipe\xyz.polytread.cli.test.{}",
                uuid::Uuid::new_v4().simple()
            );
            let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
            let server = create_server(&pipe_name).expect("create test shutdown pipe");
            let task = tokio::spawn(serve(server, shutdown_tx));

            assert!(
                request_shutdown_at(&pipe_name)
                    .await
                    .expect("request local shutdown")
            );
            assert_eq!(shutdown_rx.recv().await, Some(()));
            task.await
                .expect("shutdown pipe task")
                .expect("serve shutdown pipe");
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::Result;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    pub fn spawn_shutdown_listener(
        _shutdown_tx: mpsc::Sender<()>,
    ) -> Result<Option<JoinHandle<()>>> {
        Ok(None)
    }

    pub async fn request_shutdown() -> Result<bool> {
        Ok(false)
    }
}

pub use platform::{request_shutdown, spawn_shutdown_listener};
