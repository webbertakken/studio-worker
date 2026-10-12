//! A chat task's pictures fetched over HTTP the way the studio serves them: a job-input URL
//! (`/glimpse/api/job-input/:pictureId/:jobId/:index`) answering `image/webp` with no
//! credentials.  Driven against `wiremock`; no GPU, no studio.

use studio_worker::vision::{self, MAX_IMAGE_BYTES};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const WEBP: &[u8] = include_bytes!("fixtures/lossy-vp8.webp");

/// `reqwest::blocking` runs its own runtime; keep it off the test's tokio runtime.
fn detached<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::spawn(f)
        .join()
        .expect("worker thread panicked")
}

async fn serving(route: &str, response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_job_input_picture_is_fetched_and_decoded() {
    let route = "/glimpse/api/job-input/p-1/j-1/0";
    let server = serving(
        route,
        ResponseTemplate::new(200)
            .insert_header("content-type", "image/webp")
            .set_body_bytes(WEBP),
    )
    .await;
    let url = format!("{}{route}", server.uri());
    let images = detached(move || vision::load_all(&[url])).unwrap();
    assert_eq!(images.len(), 1);
    assert!(images[0].width > 0 && images[0].height > 0);
}

#[tokio::test]
async fn a_picture_the_studio_no_longer_serves_fails_the_read() {
    let route = "/glimpse/api/job-input/p-1/j-gone/0";
    let server = serving(route, ResponseTemplate::new(404)).await;
    let url = format!("{}{route}", server.uri());
    let err = detached(move || vision::fetch(&url)).unwrap_err();
    assert!(err.to_string().contains("HTTP 404"), "{err}");
}

#[tokio::test]
async fn a_picture_larger_than_the_bound_is_refused() {
    let route = "/big";
    let server = serving(
        route,
        ResponseTemplate::new(200).set_body_bytes(vec![0u8; MAX_IMAGE_BYTES as usize + 1]),
    )
    .await;
    let url = format!("{}{route}", server.uri());
    let err = detached(move || vision::fetch(&url)).unwrap_err();
    assert!(err.to_string().contains("bytes; at most"), "{err}");
}

#[tokio::test]
async fn an_empty_picture_is_refused() {
    let route = "/empty";
    let server = serving(route, ResponseTemplate::new(200)).await;
    let url = format!("{}{route}", server.uri());
    let err = detached(move || vision::fetch(&url)).unwrap_err();
    assert!(err.to_string().contains("is empty"), "{err}");
}
