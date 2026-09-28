use super::*;

use tokio::net::TcpListener;

/// A call to a listener that never answers, or to a port nobody listens on.
async fn failed(listening: bool) -> Result<(), Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    if !listening {
        drop(listener);
    }
    let call = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_millis(100));
    Err(Error::Network(call.send().await.unwrap_err()))
}

// contract: C12
#[tokio::test]
async fn a_timed_out_call_opens_a_growing_window_that_an_answer_closes() {
    let (link, silent, refused) = (Link::default(), failed(true).await, failed(false).await);
    link.record(&refused);
    assert!(link.check().is_ok(), "a refused connection opens no window");
    link.record(&silent);
    assert!(matches!(link.check(), Err(Error::Offline)));
    assert!(link.remaining().is_some_and(|left| left <= FIRST_WINDOW));
    link.record(&refused);
    assert!(link.remaining().is_some_and(|left| left <= FIRST_WINDOW));
    link.record(&silent);
    assert!(link.remaining().is_some_and(|left| left > FIRST_WINDOW));
    link.record(&Ok(()));
    assert!(link.check().is_ok() && link.remaining().is_none());
}
