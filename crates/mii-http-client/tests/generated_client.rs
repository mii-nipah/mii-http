mii_http_client::client! {
    pub struct SampleApi;
    spec = "../../examples/sample.http";

    GET /status as status => String;
    GET /greet as greet => String;
    POST /echo as echo => String;
    GET /users/:user_id as user => String;
    POST /submit-form as submit_form => String;
    POST /submit-json as submit_json => serde_json::Value;
    GET /headers as headers => mii_http_client::ByteStream;
}

mii_http_client::client! {
    pub struct UploadApi;
    spec = "tests/upload.http";

    POST /upload as upload => String;
}

#[test]
fn generated_request_types_are_usable() {
    let api = SampleApi::new("http://localhost:8080")
        .expect("generated client should accept a normal base URL")
        .bearer_token("token");

    let _ = api.inner();
    let _greet = GreetRequest {
        name: "nipah".into(),
        guest: None,
    };
    let _user = UserRequest {
        user_id: mii_http_client::uuid::Uuid::nil(),
    };
    let _form = SubmitFormRequest {
        body: SubmitFormBody {
            username: "nipah".into(),
            age: Some(33),
        },
    };
    let _json = SubmitJsonRequest {
        body: SubmitJsonBody {
            title: "hello".into(),
            count: None,
        },
    };
    let _headers = HeadersRequest {
        x_custom_header: "x".into(),
        x_optional_header: None,
    };
    let _echo = EchoRequest {
        body: "hello".into(),
    };
    let _upload = UploadRequest {
        body: UploadBody {
            title: "cover".into(),
            file: mii_http_client::FilePart::path("Cargo.toml"),
            preview: Some(
                mii_http_client::FilePart::bytes(vec![1, 2, 3])
                    .with_file_name("preview.bin")
                    .with_mime("application/octet-stream"),
            ),
        },
    };
}
