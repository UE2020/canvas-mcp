use chrono::{Local, Months};
use directories::ProjectDirs;
use reqwest::header::{ACCEPT, CONTENT_TYPE, COOKIE, LINK, USER_AGENT};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use thirtyfour::extensions::query::ElementPollerWithTimeout;
use thirtyfour::prelude::*;
use tokio::sync::RwLock;

const CANVAS_URL_ENV: &str = "CANVAS_URL";
const HTTP_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

fn configured_canvas_url() -> anyhow::Result<reqwest::Url> {
    let value = std::env::var(CANVAS_URL_ENV)
        .map_err(|_| anyhow::anyhow!("set {CANVAS_URL_ENV} to your Canvas URL"))?;
    let url = reqwest::Url::parse(&value)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || url.path() != "/" {
        anyhow::bail!("{CANVAS_URL_ENV} must be an HTTP(S) origin without a path");
    }
    Ok(url)
}

fn chrome_profile_dir() -> anyhow::Result<std::path::PathBuf> {
    let project = ProjectDirs::from("", "", "canvas-mcp")
        .ok_or_else(|| anyhow::anyhow!("could not locate the system configuration directory"))?;
    Ok(project.config_dir().join("chrome-profile"))
}

fn next_link(header: Option<&reqwest::header::HeaderValue>) -> anyhow::Result<Option<String>> {
    let Some(header) = header else {
        return Ok(None);
    };
    let header = header.to_str()?;
    for part in header.split(',') {
        if !part.split(';').any(|value| value.trim() == "rel=\"next\"") {
            continue;
        }
        let start = part
            .find('<')
            .ok_or_else(|| anyhow::anyhow!("malformed Canvas Link header"))?
            + 1;
        let end = part[start..]
            .find('>')
            .ok_or_else(|| anyhow::anyhow!("malformed Canvas Link header"))?
            + start;
        return Ok(Some(part[start..end].to_owned()));
    }
    Ok(None)
}

fn canvas_resource_path(canvas_origin: &reqwest::Url, url: &str) -> anyhow::Result<String> {
    let url = if url.starts_with("http://") || url.starts_with("https://") {
        reqwest::Url::parse(url)?
    } else {
        canvas_origin.join(url)?
    };
    if url.scheme() != canvas_origin.scheme()
        || url.host_str() != canvas_origin.host_str()
        || url.port_or_known_default() != canvas_origin.port_or_known_default()
    {
        anyhow::bail!("attachment URL must belong to {canvas_origin}");
    }

    let mut path = url.path().trim_start_matches('/').to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    if path.is_empty() || path.contains('\\') {
        anyhow::bail!("invalid Canvas attachment URL");
    }
    Ok(path)
}

fn validate_numeric_id(kind: &str, id: &str) -> anyhow::Result<()> {
    if id.is_empty() || !id.chars().all(|value| value.is_ascii_digit()) {
        anyhow::bail!("Canvas {kind} ID must be numeric");
    }
    Ok(())
}

fn attachment_url(canvas_origin: &reqwest::Url, resource: &str) -> anyhow::Result<reqwest::Url> {
    let path = resource
        .strip_prefix("canvas://")
        .or_else(|| resource.strip_prefix("canvas-text://"))
        .ok_or_else(|| anyhow::anyhow!("unsupported resource URI: {resource}"))?;
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        anyhow::bail!("invalid Canvas resource URI: {resource}");
    }

    // A resource payload can be an absolute URL, even without a leading slash.
    // Validate the resolved origin before attaching session cookies.
    let url = canvas_origin.join(path)?;
    canvas_resource_path(canvas_origin, url.as_str())?;
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("Canvas attachment URL must not contain credentials");
    }
    Ok(url)
}

fn canvas_api_path(canvas_origin: &reqwest::Url, segments: &[&str]) -> anyhow::Result<String> {
    let mut url = canvas_origin.clone();
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Canvas URL cannot contain path segments"))?;
        path.clear();
        path.extend(["api", "v1"]);
        path.extend(segments.iter().copied());
    }
    Ok(url.path().to_owned())
}

async fn canvas_driver(canvas_origin: &reqwest::Url, headless: bool) -> anyhow::Result<WebDriver> {
    let profile = chrome_profile_dir()?;
    std::fs::create_dir_all(&profile)?;
    let mut caps = DesiredCapabilities::chrome();
    caps.add_arg(&format!("--user-data-dir={}", profile.display()))?;
    if headless {
        caps.set_headless()?;
    }

    let driver = WebDriver::managed(caps)
        .poller(Arc::new(ElementPollerWithTimeout::new(
            Duration::from_millis(5000),
            Duration::from_millis(500),
        )))
        .await?;
    if let Err(error) = driver.goto(canvas_origin.as_str()).await {
        let _ = driver.quit().await;
        return Err(error.into());
    }
    Ok(driver)
}

// Only browser operations need exclusive access to the shared Chrome profile.
// The OS releases this lock when the file is dropped, including on process exit.
async fn lock_chrome_profile() -> anyhow::Result<std::fs::File> {
    tokio::task::spawn_blocking(|| {
        let profile = chrome_profile_dir()?;
        std::fs::create_dir_all(&profile)?;
        let lock = std::fs::File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(profile.with_extension("lock"))?;
        lock.lock()?;
        Ok(lock)
    })
    .await?
}

async fn login(canvas_origin: &reqwest::Url) -> anyhow::Result<()> {
    let driver = canvas_driver(canvas_origin, false).await?;

    while driver
        .windows()
        .await
        .is_ok_and(|handles| !handles.is_empty())
    {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let _ = driver.quit().await;
    Ok(())
}

pub async fn interactive_login() -> anyhow::Result<()> {
    let canvas_origin = configured_canvas_url()?;
    let _profile_lock = lock_chrome_profile().await?;
    login(&canvas_origin).await
}

// Caller holds the profile lock until Chrome has finished shutting down.
async fn load_cookie_header(canvas_origin: &reqwest::Url) -> anyhow::Result<String> {
    let driver = canvas_driver(canvas_origin, true).await?;
    let cookies = driver.get_all_cookies().await;
    let shutdown = driver.quit().await;
    let cookies = cookies?;
    shutdown?;
    Ok(cookies
        .into_iter()
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect::<Vec<_>>()
        .join("; "))
}

#[derive(Clone)]
pub struct CanvasApi {
    canvas_origin: reqwest::Url,
    cookie_header: Arc<RwLock<String>>,
    client: reqwest::Client,
}

impl CanvasApi {
    pub async fn new() -> anyhow::Result<Self> {
        let canvas_origin = configured_canvas_url()?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        let cookie_header = {
            let _profile_lock = lock_chrome_profile().await?;
            load_cookie_header(&canvas_origin).await?
        };
        Ok(Self {
            canvas_origin,
            cookie_header: Arc::new(RwLock::new(cookie_header)),
            client,
        })
    }

    async fn canvas_cookie_header(&self) -> anyhow::Result<String> {
        let cookie_header = self.cookie_header.read().await;
        if cookie_header.is_empty() {
            anyhow::bail!(
                "no Canvas cookies are available; sign in with the configured browser profile"
            );
        }
        Ok(cookie_header.clone())
    }

    pub async fn authenticate(&self) -> anyhow::Result<AuthenticatedUser> {
        {
            let _profile_lock = lock_chrome_profile().await?;
            login(&self.canvas_origin).await?;
            let cookies = load_cookie_header(&self.canvas_origin).await?;
            *self.cookie_header.write().await = cookies;
        }
        self.api_get("/api/v1/users/self/profile", &[]).await
    }

    async fn api_response(
        &self,
        url: reqwest::Url,
        cookie: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let origin = &self.canvas_origin;
        let is_canvas_api = |url: &reqwest::Url| {
            url.scheme() == origin.scheme()
                && url.host_str() == origin.host_str()
                && url.port_or_known_default() == origin.port_or_known_default()
                && url.path().starts_with("/api/v1/")
        };
        if !is_canvas_api(&url) {
            anyhow::bail!("unexpected Canvas API URL: {url}");
        }

        let response = self
            .client
            .get(url)
            .header(USER_AGENT, HTTP_USER_AGENT)
            .header(ACCEPT, "application/json")
            .header(COOKIE, cookie)
            .send()
            .await?
            .error_for_status()?;
        if !is_canvas_api(response.url()) {
            anyhow::bail!(
                "Canvas API redirected away from Canvas; the browser session may have expired. Direct the user to authenticate using the authentication tool"
            );
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !content_type
            .to_ascii_lowercase()
            .starts_with("application/json")
        {
            anyhow::bail!(
                "Canvas API returned {content_type:?} instead of JSON; the browser session may have expired. Direct the user to authenticate using the authentication tool"
            );
        }
        Ok(response)
    }

    async fn api_get_paginated<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> anyhow::Result<Vec<T>> {
        const MAX_PAGES: usize = 100;

        if !path.starts_with("/api/v1/") {
            anyhow::bail!("Canvas API path must begin with /api/v1/");
        }

        let mut next_url = self.canvas_origin.join(path)?;
        next_url
            .query_pairs_mut()
            .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        let cookie_header = self.canvas_cookie_header().await?;
        let mut seen = HashSet::new();
        let mut items = Vec::new();

        for _ in 0..MAX_PAGES {
            if !seen.insert(next_url.as_str().to_owned()) {
                anyhow::bail!("Canvas pagination returned a repeated next URL");
            }

            let response = self.api_response(next_url, &cookie_header).await?;
            let next = next_link(response.headers().get(LINK))?;
            let mut page: Vec<T> = serde_json::from_slice(&response.bytes().await?)?;
            items.append(&mut page);

            let Some(next) = next else {
                return Ok(items);
            };
            next_url = reqwest::Url::parse(&next)?;
        }

        anyhow::bail!("Canvas API pagination exceeded {MAX_PAGES} pages")
    }

    async fn api_get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> anyhow::Result<T> {
        if !path.starts_with("/api/v1/") {
            anyhow::bail!("Canvas API path must begin with /api/v1/");
        }

        let mut url = self.canvas_origin.join(path)?;
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        let cookie_header = self.canvas_cookie_header().await?;
        let response = self.api_response(url, &cookie_header).await?;
        Ok(serde_json::from_slice(&response.bytes().await?)?)
    }

    pub async fn assignment_info(
        &self,
        course_id: &str,
        assignment_id: &str,
    ) -> anyhow::Result<AssignmentInfo> {
        validate_numeric_id("course", course_id)?;
        validate_numeric_id("assignment", assignment_id)?;
        let mut assignment: AssignmentInfo = self
            .api_get(
                &format!("/api/v1/courses/{course_id}/assignments/{assignment_id}"),
                &[("include[]", "submission".into())],
            )
            .await?;
        assignment.prepare_resources(&self.canvas_origin)?;
        Ok(assignment)
    }

    pub async fn course_info(&self, course_id: &str) -> anyhow::Result<CourseInfo> {
        validate_numeric_id("course", course_id)?;
        self.api_get(
            &format!("/api/v1/courses/{course_id}"),
            &[
                ("include[]", "syllabus_body".into()),
                ("include[]", "term".into()),
                ("include[]", "teachers".into()),
                ("include[]", "total_scores".into()),
            ],
        )
        .await
    }

    pub async fn page_info(&self, course_id: &str, page_url: &str) -> anyhow::Result<PageInfo> {
        validate_numeric_id("course", course_id)?;
        if page_url.is_empty() {
            anyhow::bail!("Canvas page URL must not be empty");
        }
        let path = canvas_api_path(
            &self.canvas_origin,
            &["courses", course_id, "pages", page_url],
        )?;
        self.api_get(&path, &[]).await
    }

    pub async fn file_info(&self, file_id: &str) -> anyhow::Result<FileInfo> {
        validate_numeric_id("file", file_id)?;
        let mut file: FileInfo = self
            .api_get(&format!("/api/v1/files/{file_id}"), &[])
            .await?;
        file.prepare_resources(&self.canvas_origin)?;
        Ok(file)
    }

    pub async fn attachment(&self, resource: &str) -> anyhow::Result<AttachmentContents> {
        let url = attachment_url(&self.canvas_origin, resource)?;
        let cookie_header = self.canvas_cookie_header().await?;
        let mut request = self.client.get(url).header(USER_AGENT, HTTP_USER_AGENT);
        if !cookie_header.is_empty() {
            request = request.header(COOKIE, cookie_header);
        }

        let response = request.send().await?.error_for_status()?;
        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_owned());
        let bytes = response.bytes().await?.to_vec();

        Ok(AttachmentContents { bytes, mime_type })
    }

    pub async fn dashboard_items(
        &self,
        query: &PlannerItemsQuery,
    ) -> anyhow::Result<Vec<DashboardItem>> {
        let params = planner_params(query, Local::now().date_naive());
        let items: Vec<PlannerItem> = self
            .api_get_paginated("/api/v1/planner/items", &params)
            .await?;
        Ok(items.into_iter().map(Into::into).collect())
    }

    pub async fn course_list(&self) -> anyhow::Result<Vec<CourseSummary>> {
        let params = vec![
            ("include[]", "all_courses".into()),
            ("include[]", "term".into()),
            ("include[]", "teachers".into()),
            ("include[]", "total_scores".into()),
            ("include[]", "concluded".into()),
            ("per_page", "100".into()),
        ];
        let mut courses: Vec<CourseSummary> =
            self.api_get_paginated("/api/v1/courses", &params).await?;
        for course in &mut courses {
            course.prepare_urls(&self.canvas_origin);
        }
        Ok(courses)
    }

    pub async fn modules(&self, course_id: &str) -> anyhow::Result<Vec<Module>> {
        validate_numeric_id("course", course_id)?;
        let params = vec![
            ("include[]", "items".into()),
            ("include[]", "content_details".into()),
            ("per_page", "100".into()),
        ];
        let mut modules: Vec<Module> = self
            .api_get_paginated(&format!("/api/v1/courses/{course_id}/modules"), &params)
            .await?;
        for module in &mut modules {
            if module.items.is_empty() && module.items_count > 0 {
                let item_params = vec![
                    ("include[]", "content_details".into()),
                    ("per_page", "100".into()),
                ];
                module.items = self
                    .api_get_paginated(
                        &format!("/api/v1/courses/{course_id}/modules/{}/items", module.id),
                        &item_params,
                    )
                    .await?;
            }
        }
        Ok(modules)
    }

    pub async fn assignments(&self, course_id: &str) -> anyhow::Result<Vec<AssignmentSummary>> {
        validate_numeric_id("course", course_id)?;
        let params = vec![
            ("include[]", "submission".into()),
            ("order_by", "due_at".into()),
            ("per_page", "100".into()),
        ];
        self.api_get_paginated(&format!("/api/v1/courses/{course_id}/assignments"), &params)
            .await
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CanvasId {
    Number(u64),
    Text(String),
}

impl CanvasId {
    fn into_string(self) -> String {
        match self {
            Self::Number(value) => value.to_string(),
            Self::Text(value) => value,
        }
    }
}

fn id<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(CanvasId::deserialize(deserializer)?.into_string())
}

fn optional_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(Option::<CanvasId>::deserialize(deserializer)?.map(CanvasId::into_string))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthenticatedUser {
    #[serde(deserialize_with = "id")]
    id: String,
    name: String,
    short_name: Option<String>,
    sortable_name: Option<String>,
    login_id: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Module {
    #[serde(deserialize_with = "id")]
    id: String,
    name: String,
    position: Option<u64>,
    unlock_at: Option<String>,
    require_sequential_progress: Option<bool>,
    published: Option<bool>,
    state: Option<String>,
    completed_at: Option<String>,
    items_count: u64,
    #[serde(default)]
    items: Vec<ModuleItem>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModuleItem {
    #[serde(deserialize_with = "id")]
    id: String,
    #[serde(deserialize_with = "id")]
    module_id: String,
    position: Option<u64>,
    title: String,
    #[serde(rename(deserialize = "type"))]
    item_type: String,
    #[serde(default, deserialize_with = "optional_id")]
    content_id: Option<String>,
    indent: Option<u64>,
    html_url: Option<String>,
    page_url: Option<String>,
    external_url: Option<String>,
    new_tab: Option<bool>,
    completion_requirement: Option<CompletionRequirement>,
    content_details: Option<ModuleItemContentDetails>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompletionRequirement {
    #[serde(rename(deserialize = "type"))]
    requirement_type: Option<String>,
    min_score: Option<f64>,
    completed: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModuleItemContentDetails {
    points_possible: Option<f64>,
    due_at: Option<String>,
    unlock_at: Option<String>,
    lock_at: Option<String>,
    locked_for_user: Option<bool>,
    lock_explanation: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CourseSummary {
    #[serde(deserialize_with = "id")]
    id: String,
    name: Option<String>,
    course_code: Option<String>,
    workflow_state: Option<String>,
    start_at: Option<String>,
    end_at: Option<String>,
    time_zone: Option<String>,
    term: Option<CourseTerm>,
    #[serde(default)]
    teachers: Vec<CourseTeacher>,
    #[serde(default)]
    enrollments: Vec<CourseEnrollment>,
    is_favorite: Option<bool>,
    concluded: Option<bool>,
    access_restricted_by_date: Option<bool>,
    #[serde(default, skip_deserializing)]
    course_url: String,
    #[serde(default, skip_deserializing)]
    modules_url: String,
}

impl CourseSummary {
    fn prepare_urls(&mut self, canvas_origin: &reqwest::Url) {
        self.course_url = format!("{}courses/{}", canvas_origin, self.id);
        self.modules_url = format!("{}/modules", self.course_url);
    }
}

#[derive(Debug, Clone)]
pub struct AttachmentContents {
    pub bytes: Vec<u8>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssignmentInfo {
    #[serde(deserialize_with = "id")]
    id: String,
    #[serde(deserialize_with = "id")]
    course_id: String,
    name: String,
    #[serde(rename(deserialize = "description"))]
    description_html: Option<String>,
    due_at: Option<String>,
    unlock_at: Option<String>,
    lock_at: Option<String>,
    points_possible: Option<f64>,
    grading_type: Option<String>,
    #[serde(default)]
    submission_types: Vec<String>,
    #[serde(default)]
    allowed_extensions: Vec<String>,
    html_url: Option<String>,
    published: Option<bool>,
    locked_for_user: Option<bool>,
    lock_explanation: Option<String>,
    #[serde(default)]
    attachments: Vec<Attachment>,
    submission: Option<AssignmentSubmission>,
}

impl AssignmentInfo {
    fn prepare_resources(&mut self, canvas_origin: &reqwest::Url) -> anyhow::Result<()> {
        self.attachments
            .iter_mut()
            .try_for_each(|attachment| attachment.prepare_resources(canvas_origin))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssignmentSummary {
    #[serde(deserialize_with = "id")]
    id: String,
    #[serde(deserialize_with = "id")]
    course_id: String,
    name: String,
    due_at: Option<String>,
    unlock_at: Option<String>,
    lock_at: Option<String>,
    points_possible: Option<f64>,
    grading_type: Option<String>,
    #[serde(default)]
    submission_types: Vec<String>,
    html_url: Option<String>,
    published: Option<bool>,
    locked_for_user: Option<bool>,
    submission: Option<AssignmentSubmission>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PageInfo {
    #[serde(
        rename(deserialize = "page_id"),
        default,
        deserialize_with = "optional_id"
    )]
    id: Option<String>,
    url: String,
    title: String,
    #[serde(rename(deserialize = "body"))]
    body_html: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    published: Option<bool>,
    front_page: Option<bool>,
    locked_for_user: Option<bool>,
    lock_explanation: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileInfo {
    #[serde(deserialize_with = "id")]
    id: String,
    uuid: Option<String>,
    #[serde(default, deserialize_with = "optional_id")]
    folder_id: Option<String>,
    display_name: Option<String>,
    filename: String,
    #[serde(rename(deserialize = "content-type"), alias = "content_type")]
    content_type: Option<String>,
    size: Option<u64>,
    created_at: Option<String>,
    updated_at: Option<String>,
    modified_at: Option<String>,
    unlock_at: Option<String>,
    lock_at: Option<String>,
    locked: Option<bool>,
    hidden: Option<bool>,
    locked_for_user: Option<bool>,
    lock_explanation: Option<String>,
    mime_class: Option<String>,
    thumbnail_url: Option<String>,
    #[serde(skip_serializing)]
    url: String,
    #[serde(default, skip_deserializing)]
    resource: String,
    #[serde(default, skip_deserializing)]
    download_resource: String,
}

impl FileInfo {
    fn prepare_resources(&mut self, canvas_origin: &reqwest::Url) -> anyhow::Result<()> {
        let path = canvas_resource_path(canvas_origin, &self.url)?;
        self.resource = format!("canvas-text://{path}");
        self.download_resource = format!("canvas://{path}");
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CourseInfo {
    #[serde(deserialize_with = "id")]
    id: String,
    name: String,
    course_code: Option<String>,
    #[serde(default, deserialize_with = "optional_id")]
    account_id: Option<String>,
    #[serde(default, deserialize_with = "optional_id")]
    enrollment_term_id: Option<String>,
    start_at: Option<String>,
    end_at: Option<String>,
    time_zone: Option<String>,
    workflow_state: Option<String>,
    default_view: Option<String>,
    #[serde(rename(deserialize = "syllabus_body"))]
    syllabus_body_html: Option<String>,
    term: Option<CourseTerm>,
    #[serde(default)]
    teachers: Vec<CourseTeacher>,
    #[serde(default)]
    enrollments: Vec<CourseEnrollment>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CourseTerm {
    #[serde(default, deserialize_with = "optional_id")]
    id: Option<String>,
    name: Option<String>,
    start_at: Option<String>,
    end_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CourseTeacher {
    #[serde(default, deserialize_with = "optional_id")]
    id: Option<String>,
    display_name: Option<String>,
    avatar_image_url: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CourseEnrollment {
    #[serde(rename(deserialize = "type"))]
    enrollment_type: Option<String>,
    role: Option<String>,
    enrollment_state: Option<String>,
    computed_current_score: Option<f64>,
    computed_current_grade: Option<String>,
    computed_final_score: Option<f64>,
    computed_final_grade: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Attachment {
    #[serde(default, deserialize_with = "optional_id")]
    id: Option<String>,
    filename: String,
    display_name: Option<String>,
    #[serde(rename(deserialize = "content-type"), alias = "content_type")]
    content_type: Option<String>,
    size: Option<u64>,
    #[serde(skip_serializing)]
    url: String,
    #[serde(default, skip_deserializing)]
    resource: String,
    #[serde(default, skip_deserializing)]
    download_resource: String,
}

impl Attachment {
    fn prepare_resources(&mut self, canvas_origin: &reqwest::Url) -> anyhow::Result<()> {
        let path = canvas_resource_path(canvas_origin, &self.url)?;
        self.resource = format!("canvas-text://{path}");
        self.download_resource = format!("canvas://{path}");
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssignmentSubmission {
    #[serde(default, deserialize_with = "optional_id")]
    id: Option<String>,
    attempt: Option<u64>,
    submitted_at: Option<String>,
    workflow_state: Option<String>,
    score: Option<f64>,
    grade: Option<String>,
    late: Option<bool>,
    missing: Option<bool>,
    excused: Option<bool>,
    graded_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardItem {
    pub course_id: Option<String>,
    pub course: Option<String>,
    pub item_type: String,
    pub item_id: Option<String>,
    pub title: String,
    pub scheduled_at: Option<String>,
    pub due_at: Option<String>,
    pub lock_at: Option<String>,
    pub points_possible: Option<f64>,
    pub html_url: Option<String>,
    pub new_activity: bool,
    pub submitted: Option<bool>,
    pub missing: Option<bool>,
    pub late: Option<bool>,
    pub graded: Option<bool>,
    pub excused: Option<bool>,
    pub has_feedback: Option<bool>,
    pub marked_complete: Option<bool>,
    pub dismissed: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct PlannerItemsQuery {
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub course_ids: Vec<String>,
    pub filter: Option<String>,
    pub per_page: Option<usize>,
}

fn planner_params(
    query: &PlannerItemsQuery,
    today: chrono::NaiveDate,
) -> Vec<(&'static str, String)> {
    let mut params = vec![(
        "per_page",
        query.per_page.unwrap_or(100).clamp(1, 100).to_string(),
    )];
    match (&query.start_date, &query.end_date) {
        (None, None) => {
            params.push(("start_date", today.to_string()));
            params.push((
                "end_date",
                today
                    .checked_add_months(Months::new(1))
                    .expect("current date supports adding one month")
                    .to_string(),
            ));
        }
        (start, end) => {
            params.extend(start.iter().map(|date| ("start_date", date.clone())));
            params.extend(end.iter().map(|date| ("end_date", date.clone())));
        }
    }
    params.extend(
        query
            .course_ids
            .iter()
            .map(|id| ("context_codes[]", format!("course_{id}"))),
    );
    params.extend(query.filter.iter().map(|value| ("filter", value.clone())));
    params
}

#[derive(Deserialize)]
struct PlannerItem {
    #[serde(default, deserialize_with = "optional_id")]
    course_id: Option<String>,
    #[serde(default, deserialize_with = "optional_id")]
    plannable_id: Option<String>,
    plannable_type: Option<String>,
    context_name: Option<String>,
    plannable_date: Option<String>,
    html_url: Option<String>,
    #[serde(default)]
    new_activity: bool,
    plannable: Plannable,
    submissions: Option<PlannerSubmissions>,
    planner_override: Option<PlannerOverride>,
}

#[derive(Deserialize)]
struct Plannable {
    title: Option<String>,
    name: Option<String>,
    due_at: Option<String>,
    lock_at: Option<String>,
    points_possible: Option<f64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PlannerSubmissions {
    Status(SubmissionStatus),
    None(bool),
}

#[derive(Deserialize)]
struct SubmissionStatus {
    submitted: Option<bool>,
    missing: Option<bool>,
    late: Option<bool>,
    graded: Option<bool>,
    excused: Option<bool>,
    #[serde(alias = "with_feedback")]
    has_feedback: Option<bool>,
}

#[derive(Deserialize)]
struct PlannerOverride {
    marked_complete: Option<bool>,
    dismissed: Option<bool>,
}

impl From<PlannerItem> for DashboardItem {
    fn from(item: PlannerItem) -> Self {
        let submission = match item.submissions {
            Some(PlannerSubmissions::Status(status)) => Some(status),
            Some(PlannerSubmissions::None(value)) => {
                debug_assert!(!value, "Canvas uses false when no submission exists");
                None
            }
            None => None,
        };
        Self {
            course_id: item.course_id,
            course: item.context_name,
            item_type: item.plannable_type.unwrap_or_else(|| "unknown".into()),
            item_id: item.plannable_id,
            title: item
                .plannable
                .title
                .or(item.plannable.name)
                .unwrap_or_else(|| "Untitled Canvas item".into()),
            scheduled_at: item.plannable_date,
            due_at: item.plannable.due_at,
            lock_at: item.plannable.lock_at,
            points_possible: item.plannable.points_possible,
            html_url: item.html_url,
            new_activity: item.new_activity,
            submitted: submission.as_ref().and_then(|value| value.submitted),
            missing: submission.as_ref().and_then(|value| value.missing),
            late: submission.as_ref().and_then(|value| value.late),
            graded: submission.as_ref().and_then(|value| value.graded),
            excused: submission.as_ref().and_then(|value| value.excused),
            has_feedback: submission.and_then(|value| value.has_feedback),
            marked_complete: item
                .planner_override
                .as_ref()
                .and_then(|value| value.marked_complete),
            dismissed: item.planner_override.and_then(|value| value.dismissed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_origin() -> reqwest::Url {
        reqwest::Url::parse("https://canvas.example.edu").unwrap()
    }

    #[tokio::test]
    async fn requests_use_cached_cookies_shared_across_clones() {
        let api = CanvasApi {
            canvas_origin: example_origin(),
            cookie_header: Arc::new(RwLock::new(String::new())),
            client: reqwest::Client::new(),
        };
        let clone = api.clone();
        assert!(clone.canvas_cookie_header().await.is_err());

        *api.cookie_header.write().await = "session=first".into();
        assert_eq!(clone.canvas_cookie_header().await.unwrap(), "session=first");

        *api.cookie_header.write().await = "session=refreshed".into();
        assert_eq!(
            clone.canvas_cookie_header().await.unwrap(),
            "session=refreshed"
        );
    }

    #[test]
    fn attachment_resources_preserve_canvas_download_urls() {
        for prefix in ["canvas://", "canvas-text://"] {
            let url = attachment_url(
                &example_origin(),
                &format!("{prefix}files/99/download?download_frd=1&verifier=example"),
            )
            .unwrap();
            assert_eq!(
                url.as_str(),
                "https://canvas.example.edu/files/99/download?download_frd=1&verifier=example"
            );
        }
    }

    #[test]
    fn attachment_resources_reject_foreign_origins_before_using_cookies() {
        for prefix in ["canvas://", "canvas-text://"] {
            for payload in [
                "https://other.example/collect",
                "https://canvas.example.edu.other.example/collect",
                "https://canvas.example.edu@other.example/collect",
                "http://canvas.example.edu/files/99",
                "https://canvas.example.edu:444/files/99",
                "//other.example/collect",
                " https://other.example/collect",
                "\thttps://other.example/collect",
                "https:\\other.example\\collect",
                "file:///etc/passwd",
                "https://user:password@canvas.example.edu/files/99",
                "",
            ] {
                assert!(
                    attachment_url(&example_origin(), &format!("{prefix}{payload}")).is_err(),
                    "accepted unsafe resource payload: {payload:?}"
                );
            }
        }
    }

    #[test]
    fn parses_next_pagination_link() {
        let header = reqwest::header::HeaderValue::from_static(
            r#"<https://canvas.example.edu/api/v1/planner/items?page=1>; rel="current", <https://canvas.example.edu/api/v1/planner/items?page=2>; rel="next""#,
        );
        assert_eq!(
            next_link(Some(&header)).unwrap().as_deref(),
            Some("https://canvas.example.edu/api/v1/planner/items?page=2")
        );
    }

    #[test]
    fn planner_defaults_to_one_calendar_month() {
        let params = planner_params(
            &PlannerItemsQuery::default(),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 31).unwrap(),
        );

        assert!(params.contains(&("start_date", "2026-01-31".into())));
        assert!(params.contains(&("end_date", "2026-02-28".into())));
    }

    #[test]
    fn planner_preserves_an_explicit_partial_range() {
        let params = planner_params(
            &PlannerItemsQuery {
                start_date: Some("2026-09-01".into()),
                ..Default::default()
            },
            chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
        );

        assert!(params.contains(&("start_date", "2026-09-01".into())));
        assert!(!params.iter().any(|(key, _)| *key == "end_date"));
    }

    #[test]
    fn deserializes_numeric_and_string_ids() {
        let numeric: ModuleItem = serde_json::from_value(serde_json::json!({
            "id": 10, "module_id": "20", "title": "Syllabus",
            "type": "File", "content_id": 30
        }))
        .unwrap();

        assert_eq!(numeric.id, "10");
        assert_eq!(numeric.module_id, "20");
        assert_eq!(numeric.content_id.as_deref(), Some("30"));

        let output = serde_json::to_value(numeric).unwrap();
        assert_eq!(output["item_type"], "File");
        assert!(output.get("type").is_none());
    }

    #[test]
    fn preserves_mcp_field_names() {
        let page: PageInfo = serde_json::from_value(serde_json::json!({
            "page_id": 5,
            "url": "syllabus",
            "title": "Syllabus",
            "body": "<p>Textbook</p>"
        }))
        .unwrap();
        let output = serde_json::to_value(page).unwrap();

        assert_eq!(output["id"], "5");
        assert_eq!(output["body_html"], "<p>Textbook</p>");
        assert!(output.get("page_id").is_none());
        assert!(output.get("body").is_none());
    }

    #[test]
    fn flattens_planner_items() {
        let raw: PlannerItem = serde_json::from_value(serde_json::json!({
            "course_id": "42",
            "plannable_id": "99",
            "plannable_type": "assignment",
            "context_name": "Example Course",
            "new_activity": true,
            "submissions": {"submitted": false, "missing": true},
            "planner_override": {"marked_complete": false},
            "plannable": {
                "title": "Example Assignment",
                "due_at": "2026-09-12T03:59:59Z",
                "points_possible": 100
            }
        }))
        .unwrap();
        let item: DashboardItem = raw.into();

        assert_eq!(item.course_id.as_deref(), Some("42"));
        assert_eq!(item.title, "Example Assignment");
        assert_eq!(item.missing, Some(true));
        assert_eq!(item.marked_complete, Some(false));
    }

    #[test]
    fn prepares_course_and_file_links() {
        let mut course: CourseSummary = serde_json::from_value(serde_json::json!({
            "id": 42,
            "name": "Example Course",
            "access_restricted_by_date": false
        }))
        .unwrap();
        course.prepare_urls(&example_origin());

        let mut file: FileInfo = serde_json::from_value(serde_json::json!({
            "id": 99,
            "filename": "example.pdf",
            "content-type": "application/pdf",
            "url": "https://canvas.example.edu/files/99/download?download_frd=1"
        }))
        .unwrap();
        file.prepare_resources(&example_origin()).unwrap();

        assert_eq!(
            course.modules_url,
            "https://canvas.example.edu/courses/42/modules"
        );
        assert_eq!(
            file.resource,
            "canvas-text://files/99/download?download_frd=1"
        );
    }

    #[test]
    fn rejects_off_origin_file_links() {
        let mut file: FileInfo = serde_json::from_value(serde_json::json!({
            "id": 1,
            "filename": "unsafe.pdf",
            "url": "https://example.com/unsafe.pdf"
        }))
        .unwrap();

        assert!(file.prepare_resources(&example_origin()).is_err());
    }

    #[test]
    fn encodes_page_slugs() {
        assert_eq!(
            canvas_api_path(
                &example_origin(),
                &["courses", "42", "pages", "week 1/readings"]
            )
            .unwrap(),
            "/api/v1/courses/42/pages/week%201%2Freadings"
        );
    }
}
