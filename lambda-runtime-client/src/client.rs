use error::{ApiError, ErrorResponse, RuntimeApiError};
use hyper::{
    client::HttpConnector,
    header::{self, HeaderMap, HeaderValue},
    rt::{Future, Stream},
    Body, Client, Method, Request, Uri,
};
use serde_json;
use std::{collections::HashMap, fmt};
use tokio::runtime::Runtime;

const RUNTIME_API_VERSION: &str = "2018-06-01";
const API_CONTENT_TYPE: &str = "application/json";
const API_ERROR_CONTENT_TYPE: &str = "application/vnd.aws.lambda.error+json";
const RUNTIME_ERROR_HEADER: &str = "Lambda-Runtime-Function-Error-Type";

/// Enum of the headers returned by Lambda's `/next` API call.
pub enum LambdaHeaders {
    /// The AWS request ID
    RequestId,
    /// The ARN of the Lambda function being invoked
    FunctionArn,
    /// The X-Ray trace ID generated for this invocation
    TraceId,
    /// The deadline for the function execution in nanoseconds
    Deadline,
    /// The client context header. This field is populated when the function
    /// is invoked from a mobile client.
    ClientContext,
    /// The Cognito Identity context for the invocation. This field is populated
    /// when the function is invoked with AWS credentials obtained from Cognito
    /// Identity.
    CognitoIdentity,
}

impl fmt::Display for LambdaHeaders {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LambdaHeaders::RequestId => write!(f, "Lambda-Runtime-Aws-Request-Id"),
            LambdaHeaders::FunctionArn => write!(f, "Lambda-Runtime-Invoked-Function-Arn"),
            LambdaHeaders::TraceId => write!(f, "Lambda-Runtime-Trace-Id"),
            LambdaHeaders::Deadline => write!(f, "Lambda-Runtime-Deadline-Ms"),
            LambdaHeaders::ClientContext => write!(f, "Lambda-Runtime-Client-Context"),
            LambdaHeaders::CognitoIdentity => write!(f, "Lambda-Runtime-Cognito-Identity"),
        }
    }
}

/// AWS Moble SDK client properties
#[derive(Deserialize, Clone)]
pub struct ClientApplication {
    /// The mobile app installation id
    #[serde(rename = "installationId")]
    pub installation_id: String,
    /// The app title for the mobile app as registered with AWS' mobile services.
    #[serde(rename = "appTitle")]
    pub app_title: String,
    /// The version name of the application as registered with AWS' mobile services.
    #[serde(rename = "appVersionName")]
    pub app_version_name: String,
    /// The app version code.
    #[serde(rename = "appVersionCode")]
    pub app_version_code: String,
    /// The package name for the mobile application invoking the function
    #[serde(rename = "appPackageName")]
    pub app_package_name: String,
}

/// Client context sent by the AWS Mobile SDK.
#[derive(Deserialize, Clone)]
pub struct ClientContext {
    /// Information about the mobile application invoking the function.
    pub client: ClientApplication,
    /// Custom properties attached to the mobile event context.
    pub custom: HashMap<String, String>,
    /// Environment settings from the mobile client.
    pub environment: HashMap<String, String>,
}

#[derive(Deserialize, Clone)]
/// Cognito identity information sent with the event
pub struct CognitoIdentity {
    /// The unique identity id for the Cognito credentials invoking the function.
    pub identity_id: String,
    /// The identity pool id the caller is "registered" with.
    pub identity_pool_id: String,
}

/// The Lambda function execution context. The values in this struct
/// are populated using the [Lambda environment variables](https://docs.aws.amazon.com/lambda/latest/dg/current-supported-versions.html)
/// and the headers returned by the poll request to the Runtime APIs.
/// A new instance of the `Context` object is passed to each handler invocation.
#[derive(Clone)]
pub struct EventContext {
    /// The ARN of the Lambda function being invoked.
    pub invoked_function_arn: String,
    /// The AWS request ID generated by the Lambda service.
    pub aws_request_id: String,
    /// The X-Ray trace ID for the current invocation.
    pub xray_trace_id: String,
    /// The execution deadline for the current invocation in nanoseconds.
    pub deadline: u128,
    /// The client context object sent by the AWS mobile SDK. This field is
    /// empty unless the function is invoked using an AWS mobile SDK.
    pub client_context: Option<ClientContext>,
    /// The Cognito identity that invoked the function. This field is empty
    /// unless the invocation request to the Lambda APIs was made using AWS
    /// credentials issues by Amazon Cognito Identity Pools.
    pub identity: Option<CognitoIdentity>,
}

/// Used by the Runtime to communicate with the internal endpoint.
pub struct RuntimeClient {
    _runtime: Runtime,
    http_client: Client<HttpConnector, Body>,
    endpoint: String,
}

impl RuntimeClient {
    /// Creates a new instance of the Runtime APIclient SDK. The http client has timeouts disabled and
    /// will always send a `Connection: keep-alive` header.
    pub fn new(endpoint: String, runtime: Option<Runtime>) -> Result<Self, ApiError> {
        debug!("Starting new HttpRuntimeClient for {}", endpoint);
        // start a tokio core main event loop for hyper
        let runtime = match runtime {
            Some(r) => r,
            None => Runtime::new()?,
        };

        let http_client = Client::builder().executor(runtime.executor()).build_http();

        Ok(RuntimeClient {
            _runtime: runtime,
            http_client,
            endpoint,
        })
    }
}

impl RuntimeClient {
    /// Polls for new events to the Runtime APIs.
    pub fn next_event(&self) -> Result<(Vec<u8>, EventContext), ApiError> {
        let uri = format!(
            "http://{}/{}/runtime/invocation/next",
            self.endpoint, RUNTIME_API_VERSION
        )
        .parse()?;
        trace!("Polling for next event");

        // We wait instead of processing the future asynchronously because AWS Lambda
        // itself enforces only one event per container at a time. No point in taking on
        // the additional complexity.
        let out = self.http_client.get(uri).wait();
        match out {
            Ok(resp) => {
                if resp.status().is_client_error() {
                    error!(
                        "Runtime API returned client error when polling for new events: {}",
                        resp.status()
                    );
                    return Err(ApiError::new(&format!(
                        "Error {} when polling for events",
                        resp.status()
                    )));
                }
                if resp.status().is_server_error() {
                    error!(
                        "Runtime API returned server error when polling for new events: {}",
                        resp.status()
                    );
                    return Err(ApiError::new("Server error when polling for new events")
                        .unrecoverable()
                        .clone());
                }
                let ctx = self.get_event_context(&resp.headers())?;
                let out = resp.into_body().concat2().wait()?;
                let buf: Vec<u8> = out.into_bytes().to_vec();

                trace!(
                    "Received new event for request id {}. Event length {} bytes",
                    ctx.aws_request_id,
                    buf.len()
                );
                Ok((buf, ctx))
            }
            Err(e) => {
                error!("Error when fetching next event from Runtime API: {}", e);
                Err(ApiError::from(e))
            }
        }
    }

    /// Calls the Lambda Runtime APIs to submit a response to an event. In this function we treat
    /// all errors from the API as an unrecoverable error. This is because the API returns
    /// 4xx errors for responses that are too long. In that case, we simply log the output and fail.
    ///
    /// # Arguments
    ///
    /// * `request_id` The request id associated with the event we are serving the response for.
    ///                This is returned as a header from the poll (`/next`) API.
    /// * `output` The object be sent back to the Runtime APIs as a response.
    ///
    /// # Returns
    /// A `Result` object containing a bool return value for the call or an `error::ApiError` instance.
    pub fn event_response(&self, request_id: &str, output: Vec<u8>) -> Result<(), ApiError> {
        let uri: Uri = format!(
            "http://{}/{}/runtime/invocation/{}/response",
            self.endpoint, RUNTIME_API_VERSION, request_id
        )
        .parse()?;
        trace!(
            "Posting response for request {} to Runtime API. Response length {} bytes",
            request_id,
            output.len()
        );
        let req = self.get_runtime_post_request(&uri, output);

        match self.http_client.request(req).wait() {
            Ok(resp) => {
                if !resp.status().is_success() {
                    error!(
                        "Error from Runtime API when posting response for request {}: {}",
                        request_id,
                        resp.status()
                    );
                    return Err(ApiError::new(&format!(
                        "Error {} while sending response",
                        resp.status()
                    )));
                }
                trace!("Posted response to Runtime API for request {}", request_id);
                Ok(())
            }
            Err(e) => {
                error!("Error when calling runtime API for request {}: {}", request_id, e);
                Err(ApiError::from(e))
            }
        }
    }

    /// Calls Lambda's Runtime APIs to send an error generated by the `Handler`. Because it's rust,
    /// the error type for lambda is always `handled`.
    ///
    /// # Arguments
    ///
    /// * `request_id` The request id associated with the event we are serving the error for.
    /// * `e` An instance of `errors::HandlerError` generated by the handler function. Handler
    ///       functions can generate a new error using the `new_error(&str)` method of the `Context`
    ///       object.
    ///
    /// # Returns
    /// A `Result` object containing a bool return value for the call or an `error::ApiError` instance.
    pub fn event_error(&self, request_id: &str, e: &RuntimeApiError) -> Result<(), ApiError> {
        let uri: Uri = format!(
            "http://{}/{}/runtime/invocation/{}/error",
            self.endpoint, RUNTIME_API_VERSION, request_id
        )
        .parse()?;
        trace!(
            "Posting error to runtime API for request {}: {}",
            request_id,
            e.to_response().error_message
        );
        let req = self.get_runtime_error_request(&uri, &e.to_response());

        match self.http_client.request(req).wait() {
            Ok(resp) => {
                if !resp.status().is_success() {
                    error!(
                        "Error from Runtime API when posting error response for request {}: {}",
                        request_id,
                        resp.status()
                    );
                    return Err(ApiError::new(&format!(
                        "Error {} while sending response",
                        resp.status()
                    )));
                }
                trace!("Posted error response for request id {}", request_id);
                Ok(())
            }
            Err(e) => {
                error!("Error when calling runtime API for request {}: {}", request_id, e);
                Err(ApiError::from(e))
            }
        }
    }

    /// Calls the Runtime APIs to report a failure during the init process.
    /// The contents of the error (`e`) parmeter are passed to the Runtime APIs
    /// using the private `to_response()` method.
    ///
    /// # Arguments
    ///
    /// * `e` An instance of `errors::RuntimeError`.
    ///
    /// # Panics
    /// If it cannot send the init error. In this case we panic to force the runtime
    /// to restart.
    pub fn fail_init(&self, e: &RuntimeApiError) {
        let uri: Uri = format!("http://{}/{}/runtime/init/error", self.endpoint, RUNTIME_API_VERSION)
            .parse()
            .expect("Could not generate Runtime URI");
        error!("Calling fail_init Runtime API: {}", e.to_response().error_message);
        let req = self.get_runtime_error_request(&uri, &e.to_response());

        self.http_client
            .request(req)
            .wait()
            .map_err(|e| {
                error!("Error while sending init failed message: {}", e);
                panic!("Error while sending init failed message: {}", e);
            })
            .map(|resp| {
                info!("Successfully sent error response to the runtime API: {:?}", resp);
            })
            .expect("Could not complete init_fail request");
    }

    /// Returns the endpoint configured for this HTTP Runtime client.
    pub fn get_endpoint(&self) -> String {
        self.endpoint.clone()
    }
}

impl RuntimeClient {
    /// Creates a Hyper `Request` object for the given `Uri` and `Body`. Sets the
    /// HTTP method to `POST` and the `Content-Type` header value to `application/json`.
    ///
    /// # Arguments
    ///
    /// * `uri` A `Uri` reference target for the request
    /// * `body` The content of the post request. This parameter must not be null
    ///
    /// # Returns
    /// A Populated Hyper `Request` object.
    fn get_runtime_post_request(&self, uri: &Uri, body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(uri.clone())
            .header(header::CONTENT_TYPE, header::HeaderValue::from_static(API_CONTENT_TYPE))
            .body(Body::from(body))
            .unwrap()
    }

    fn get_runtime_error_request(&self, uri: &Uri, e: &ErrorResponse) -> Request<Body> {
        let body = serde_json::to_vec(e).expect("Could not turn error object into response JSON");
        Request::builder()
            .method(Method::POST)
            .uri(uri.clone())
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static(API_ERROR_CONTENT_TYPE),
            )
            .header(RUNTIME_ERROR_HEADER, HeaderValue::from_static("RuntimeError")) // TODO: We should add this code to the error object.
            .body(Body::from(body))
            .unwrap()
    }

    /// Creates an `EventContext` object based on the response returned by the Runtime
    /// API `/next` endpoint.
    ///
    /// # Arguments
    ///
    /// * `resp` The response returned by the Runtime APIs endpoint.
    ///
    /// # Returns
    /// A `Result` containing the populated `EventContext` or an `ApiError` if the required headers
    /// were not present or the client context and cognito identity could not be parsed from the
    /// JSON string.
    fn get_event_context(&self, headers: &HeaderMap<HeaderValue>) -> Result<EventContext, ApiError> {
        // let headers = resp.headers();

        let aws_request_id = match headers.get(LambdaHeaders::RequestId.to_string()) {
            Some(value) => value.to_str()?.to_owned(),
            None => {
                error!("Response headers do not contain request id header");
                return Err(ApiError::new(&format!("Missing {} header", LambdaHeaders::RequestId)));
            }
        };

        let invoked_function_arn = match headers.get(LambdaHeaders::FunctionArn.to_string()) {
            Some(value) => value.to_str()?.to_owned(),
            None => {
                error!("Response headers do not contain function arn header");
                return Err(ApiError::new(&format!("Missing {} header", LambdaHeaders::FunctionArn)));
            }
        };

        let xray_trace_id = match headers.get(LambdaHeaders::FunctionArn.to_string()) {
            Some(value) => value.to_str()?.to_owned(),
            None => {
                error!("Response headers do not contain trace id header");
                return Err(ApiError::new(&format!("Missing {} header", LambdaHeaders::TraceId)));
            }
        };

        let deadline: u128 = match headers.get(LambdaHeaders::Deadline.to_string()) {
            Some(value) => value.to_str()?.to_owned(),
            None => {
                error!("Response headers do not contain deadline header");
                return Err(ApiError::new(&format!("Missing {} header", LambdaHeaders::Deadline)));
            }
        }
        .parse::<u128>()?;

        let mut ctx = EventContext {
            aws_request_id,
            invoked_function_arn,
            xray_trace_id,
            deadline,
            client_context: Option::default(),
            identity: Option::default(),
        };

        if let Some(ctx_json) = headers.get(LambdaHeaders::ClientContext.to_string()) {
            let ctx_json = ctx_json.to_str()?;
            trace!("Found Client Context in response headers: {}", ctx_json);
            let ctx_value: ClientContext = serde_json::from_str(&ctx_json)?;
            ctx.client_context = Option::from(ctx_value);
        };

        if let Some(cognito_json) = headers.get(LambdaHeaders::CognitoIdentity.to_string()) {
            let cognito_json = cognito_json.to_str()?;
            trace!("Found Cognito Identity in response headers: {}", cognito_json);
            let identity_value: CognitoIdentity = serde_json::from_str(&cognito_json)?;
            ctx.identity = Option::from(identity_value);
        };

        Ok(ctx)
    }
}
