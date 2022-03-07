use std::path::PathBuf;

use log::info;

use simple_proxy::middlewares::router::RouterConfig;
use simple_proxy::middlewares::Logger;
use simple_proxy::middlewares::Router;
use simple_proxy::Environment;
use simple_proxy::SimpleProxy;

mod common;

#[cfg(test)]
mod tests {
    use std::error::Error;

    use hyper::client::Client;
    use tokio::task::JoinHandle;
    use tokio::time::{self, Duration};

    use super::*;

    const TEST_PORT: u16 = 3456;

    #[derive(Debug, Clone)]
    pub struct Config(String);

    impl RouterConfig for Config {
        fn get_router_filename(&self) -> &str {
            self.0.as_str()
        }
    }

    fn build_test_proxy() -> SimpleProxy {
        let mut proxy = SimpleProxy::new(TEST_PORT, Environment::Development);
        let logger = Logger::new();

        let mut test_config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        test_config_path.push("tests/test_routes.json");
        let path = test_config_path.to_str().unwrap().to_string();
        let router = Router::new(&Config(path));

        proxy.add_middleware(Box::new(router));
        proxy.add_middleware(Box::new(logger));

        return proxy;
    }

    async fn start_server() -> JoinHandle<Result<(), Box<dyn Error + Send + Sync>>> {
        let server = build_test_proxy();

        info!("Starting proxy server in the background");
        tokio::spawn(async move { server.run().await })
    }

    #[tokio::test]
    async fn proxy_works() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = pretty_env_logger::try_init();

        start_server().await;

        tokio::time::sleep(Duration::from_secs(1)).await;

        let client = Client::new();

        let mut interval = time::interval(Duration::from_millis(100));
        for _ in 0..10 {
            let uri = format!("http://localhost:{}/test_from", TEST_PORT).parse()?;
            interval.tick().await;
            let resp = client.get(uri).await?;
            assert_eq!(404, resp.status().as_u16());
        }

        let mut interval = time::interval(Duration::from_millis(10));
        let mut too_many = false;
        let mut not_found = false;
        for _ in 0..50 {
            let uri = format!("http://localhost:{}/test_from", TEST_PORT).parse()?;
            interval.tick().await;
            let resp = client.get(uri).await?;
            if resp.status().as_u16() == 404 {
                not_found = true;
            }
            if resp.status().as_u16() == 429 {
                too_many = true;
            }
        }
        assert!(not_found);
        assert!(too_many);

        Ok(())
    }
}
