use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use quick_xml::{Reader, escape::unescape, events::Event};
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{
    ServerHandler, ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_handler,
    tool_router, transport::stdio,
};
use std::io::{Cursor, Read};

mod api;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AssignmentInfoParams {
    course_id: String,
    assignment_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CourseInfoParams {
    course_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PageInfoParams {
    course_id: String,
    /// Stable Canvas page URL/slug returned as page_url by module_list.
    page_url: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FileInfoParams {
    /// Stable Canvas file ID returned as content_id by module_list.
    file_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ModuleAndAssignmentListParams {
    course_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum DashboardFilter {
    NewActivity,
    IncompleteItems,
    CompleteItems,
}

impl DashboardFilter {
    fn as_str(&self) -> &'static str {
        match self {
            Self::NewActivity => "new_activity",
            Self::IncompleteItems => "incomplete_items",
            Self::CompleteItems => "complete_items",
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DashboardItemsParams {
    /// Inclusive start date in YYYY-MM-DD or ISO 8601 format. Defaults to today when both dates are omitted.
    start_date: Option<String>,
    /// Inclusive end date in YYYY-MM-DD or ISO 8601 format. Defaults to one month from today when both dates are omitted.
    end_date: Option<String>,
    /// Limit results to these Canvas course IDs.
    course_ids: Option<Vec<String>>,
    /// Limit results by Canvas planner state.
    filter: Option<DashboardFilter>,
    /// Requested Canvas page size (defaults to 100 and is clamped to 1-100).
    per_page: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AttachmentTextParams {
    resource: String,
    /// Zero-based character offset into the extracted text.
    start: Option<usize>,
    /// Maximum number of characters to return (defaults to 10,000; maximum 15,000).
    max_chars: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AttachmentImageParams {
    resource: String,
    /// One-based PDF page number.
    page: u32,
    /// One-based image number on the page, in PDF resource order.
    image: usize,
}

#[derive(Debug, serde::Serialize)]
struct PdfImageInfo {
    page: u32,
    image: usize,
    width: i64,
    height: i64,
    filters: Vec<String>,
    mime_type: Option<&'static str>,
}

const MAX_INLINE_ATTACHMENT_BYTES: usize = 24 * 1024;
const DEFAULT_TEXT_PAGE_CHARS: usize = 10_000;
const MAX_TEXT_PAGE_CHARS: usize = 15_000;
const MAX_DOCX_XML_BYTES: u64 = 16 * 1024 * 1024;

const DOCX_MIME_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

fn docx_text(bytes: &[u8]) -> Result<String, String> {
    let cursor = Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("failed to open Word document: {e}"))?;
    let mut document = archive
        .by_name("word/document.xml")
        .map_err(|e| format!("Word document has no word/document.xml: {e}"))?;
    if document.size() > MAX_DOCX_XML_BYTES {
        return Err(format!(
            "Word document XML is {} bytes; maximum supported size is {MAX_DOCX_XML_BYTES}",
            document.size()
        ));
    }

    let mut xml = String::with_capacity(document.size() as usize);
    document
        .read_to_string(&mut xml)
        .map_err(|e| format!("failed to read Word document XML: {e}"))?;

    let mut reader = Reader::from_str(&xml);
    let mut text = String::new();
    let mut text_depth = 0_u32;
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) if tag.local_name().as_ref() == b"t" => text_depth += 1,
            Ok(Event::Text(value)) if text_depth > 0 => {
                let decoded = value
                    .decode()
                    .map_err(|e| format!("failed to decode Word document text: {e}"))?;
                let decoded = unescape(&decoded)
                    .map_err(|e| format!("failed to unescape Word document text: {e}"))?;
                text.push_str(&decoded);
            }
            Ok(Event::GeneralRef(value)) if text_depth > 0 => {
                let reference = value
                    .decode()
                    .map_err(|e| format!("failed to decode Word document reference: {e}"))?;
                let encoded = format!("&{reference};");
                let decoded = unescape(&encoded)
                    .map_err(|e| format!("failed to resolve Word document reference: {e}"))?;
                text.push_str(&decoded);
            }
            Ok(Event::Empty(tag)) => match tag.local_name().as_ref() {
                b"tab" => text.push('\t'),
                b"br" | b"cr" => text.push('\n'),
                _ => {}
            },
            Ok(Event::End(tag)) => match tag.local_name().as_ref() {
                b"t" => text_depth = text_depth.saturating_sub(1),
                b"p" => text.push('\n'),
                b"tc" => text.push('\t'),
                b"tr" => text.push('\n'),
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("failed to parse Word document XML: {e}")),
            _ => {}
        }
    }

    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    Ok(text.trim().to_owned())
}

fn attachment_text(attachment: &api::AttachmentContents) -> Result<String, String> {
    let mime_type = attachment
        .mime_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if mime_type == "application/pdf" || attachment.bytes.starts_with(b"%PDF-") {
        pdf_extract::extract_text_from_mem(&attachment.bytes)
            .map_err(|e| format!("failed to extract PDF text: {e}"))
    } else if mime_type == DOCX_MIME_TYPE {
        docx_text(&attachment.bytes)
    } else if mime_type.starts_with("text/")
        || matches!(mime_type.as_str(), "application/json" | "application/xml")
    {
        String::from_utf8(attachment.bytes.clone())
            .map_err(|e| format!("attachment is not valid UTF-8 text: {e}"))
    } else {
        Err(format!(
            "unsupported attachment type {mime_type:?}; text extraction supports PDF, Word (.docx), text, JSON, and XML"
        ))
    }
}

fn image_mime_type(filters: &[String]) -> Option<&'static str> {
    if filters.iter().any(|filter| filter == "DCTDecode") {
        Some("image/jpeg")
    } else if filters.iter().any(|filter| filter == "JPXDecode") {
        Some("image/jp2")
    } else {
        None
    }
}

fn pdf_image_info(bytes: &[u8]) -> Result<Vec<PdfImageInfo>, String> {
    let document =
        lopdf::Document::load_mem(bytes).map_err(|e| format!("failed to parse PDF images: {e}"))?;
    let mut info = Vec::new();
    for (page, page_id) in document.get_pages() {
        let images = document.get_page_images(page_id).unwrap_or_default();
        for (index, image) in images.iter().enumerate() {
            let filters = image.filters.clone().unwrap_or_default();
            info.push(PdfImageInfo {
                page,
                image: index + 1,
                width: image.width,
                height: image.height,
                mime_type: image_mime_type(&filters),
                filters,
            });
        }
    }
    Ok(info)
}

fn pdf_image(bytes: &[u8], page: u32, index: usize) -> Result<(Vec<u8>, PdfImageInfo), String> {
    if page == 0 || index == 0 {
        return Err(
            "page and image numbers are one-based and must be greater than zero".to_owned(),
        );
    }
    let document =
        lopdf::Document::load_mem(bytes).map_err(|e| format!("failed to parse PDF images: {e}"))?;
    let page_id = document
        .get_pages()
        .get(&page)
        .copied()
        .ok_or_else(|| format!("PDF has no page {page}"))?;
    let images = document
        .get_page_images(page_id)
        .map_err(|e| format!("failed to read images on PDF page {page}: {e}"))?;
    let image = images.get(index - 1).ok_or_else(|| {
        format!(
            "PDF page {page} has no image {index}; it has {}",
            images.len()
        )
    })?;
    let filters = image.filters.clone().unwrap_or_default();
    let mime_type = image_mime_type(&filters).ok_or_else(|| {
        format!(
            "PDF page {page} image {index} uses unsupported filters {filters:?}; JPEG and JPEG 2000 are supported"
        )
    })?;
    let info = PdfImageInfo {
        page,
        image: index,
        width: image.width,
        height: image.height,
        filters,
        mime_type: Some(mime_type),
    };
    Ok((image.content.to_vec(), info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn one_page_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn extracts_pdf_text() {
        let attachment = api::AttachmentContents {
            bytes: one_page_pdf("Practice Set 1"),
            mime_type: Some("application/pdf".to_owned()),
        };

        assert!(
            attachment_text(&attachment)
                .unwrap()
                .contains("Practice Set 1")
        );
    }

    #[test]
    fn accepts_text_mime_parameters() {
        let attachment = api::AttachmentContents {
            bytes: b"problem one".to_vec(),
            mime_type: Some("text/plain; charset=utf-8".to_owned()),
        };

        assert_eq!(attachment_text(&attachment).unwrap(), "problem one");
    }

    fn word_document(document_xml: &str) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file(
                    "word/document.xml",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(document_xml.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        bytes.into_inner()
    }

    #[test]
    fn extracts_docx_text_and_structure() {
        let bytes = word_document(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:t>Required &amp; recommended</w:t></w:r></w:p>
                <w:p><w:r><w:t>Notebook</w:t><w:tab/><w:t>$5</w:t><w:br/><w:t>Calculator</w:t></w:r></w:p>
              </w:body>
            </w:document>"#,
        );
        let attachment = api::AttachmentContents {
            bytes,
            mime_type: Some(DOCX_MIME_TYPE.to_owned()),
        };

        assert_eq!(
            attachment_text(&attachment).unwrap(),
            "Required & recommended\nNotebook\t$5\nCalculator"
        );
    }

    #[test]
    fn rejects_malformed_docx_xml_with_clear_error() {
        let attachment = api::AttachmentContents {
            bytes: word_document("<not-xml"),
            mime_type: Some(DOCX_MIME_TYPE.to_owned()),
        };

        assert!(
            attachment_text(&attachment)
                .unwrap_err()
                .contains("failed to parse Word document XML")
        );
    }
}

#[derive(Clone)]
struct CanvasTool {
    api: api::CanvasApi,
}

#[tool_router]
impl CanvasTool {
    #[tool(
        description = "Authenticate the Canvas connector with the user's institutional login. Use this tool whenever another Canvas tool reports that cookies are missing, the session expired, or Canvas redirected to a login page. Before calling it, inform the user that the Canvas session is not authenticated and that a visible browser will open; ask them to finish signing in and close the browser window when done. The connector safely replaces its headless browser with the interactive browser and restores the headless session afterward."
    )]
    async fn auth(&self) -> Result<CallToolResult, ErrorData> {
        let user = self.api.authenticate().await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to authenticate with Canvas: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "authenticated": true,
            "user": user
        })))
    }

    #[tool(
        description = "Get a specific assignment through the read-only Canvas REST API. Pass the Canvas course ID and assignment ID. Returns structured dates, grading and submission settings, HTML instructions, all attachments, and the current user's submission status when available."
    )]
    async fn assignment_info(
        &self,
        Parameters(AssignmentInfoParams {
            course_id,
            assignment_id,
        }): Parameters<AssignmentInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let res = self
            .api
            .assignment_info(&course_id, &assignment_id)
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to fetch Canvas assignment: {e}"), None)
            })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "assignment": res
        })))
    }

    #[tool(
        description = "List the user's Canvas courses through the read-only REST API, including stable course IDs, term and teacher details, enrollment roles and grades when available, course state, and direct course/module links. Includes concluded courses so it can replace Canvas's All Courses page. Use course_id with course_info, assignment_list, or module_list."
    )]
    async fn course_list(&self) -> Result<CallToolResult, ErrorData> {
        let res = self.api.course_list().await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas courses: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "courses": res
        })))
    }

    #[tool(
        description = "List every assignment in a course through the read-only Canvas REST API. Returns assignment and course IDs, dates, points, grading and submission types, Canvas links, availability, and the current user's submission status. Use course_id and assignment id with assignment_info for full instructions and attachments."
    )]
    async fn assignment_list(
        &self,
        Parameters(p): Parameters<ModuleAndAssignmentListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let res = self.api.assignments(&p.course_id).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas assignments: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "assignments": res
        })))
    }

    #[tool(
        description = "Get structured information about a course through the read-only Canvas REST API, including its syllabus HTML, term, teachers, and the current user's enrollment grades when Canvas makes them available."
    )]
    async fn course_info(
        &self,
        Parameters(CourseInfoParams { course_id }): Parameters<CourseInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let res = self.api.course_info(&course_id).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas course: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "course": res
        })))
    }

    #[tool(
        description = "Get a Canvas course page through the read-only Pages REST API. Pass course_id and the page_url returned by module_list. Returns the page title, HTML body, publication and front-page status, timestamps, and lock information."
    )]
    async fn page_info(
        &self,
        Parameters(PageInfoParams {
            course_id,
            page_url,
        }): Parameters<PageInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let res = self
            .api
            .page_info(&course_id, &page_url)
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to fetch Canvas page: {e}"), None)
            })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "page": res
        })))
    }

    #[tool(
        description = "Get Canvas file metadata through the read-only Files REST API. Pass the file content_id returned by module_list. Returns names, type, size, timestamps, availability, and canvas-text:// and canvas:// resources for text extraction or downloading."
    )]
    async fn file_info(
        &self,
        Parameters(FileInfoParams { file_id }): Parameters<FileInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let res = self.api.file_info(&file_id).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas file: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "file": res
        })))
    }

    #[tool(
        description = "List every module and module item in a course through the read-only Canvas REST API. Returns stable module/item/content IDs, item types, links, completion requirements, availability, dates, and points. Assignment module items expose content_id for use as assignment_id with assignment_info."
    )]
    async fn module_list(
        &self,
        Parameters(p): Parameters<ModuleAndAssignmentListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let res = self.api.modules(&p.course_id).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas modules: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "modules": res
        })))
    }

    #[tool(
        description = "Get the user's Canvas planner items through the read-only Canvas REST API. Defaults to the one-month window from today when no dates are supplied, and supports explicit date ranges, course filters, and completion/activity filters. Returns course and item IDs, item type, title, scheduled and due dates, points, links, and submission status such as submitted, missing, late, graded, or excused."
    )]
    async fn dashboard_items(
        &self,
        Parameters(params): Parameters<DashboardItemsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = api::PlannerItemsQuery {
            start_date: params.start_date,
            end_date: params.end_date,
            course_ids: params.course_ids.unwrap_or_default(),
            filter: params.filter.map(|value| value.as_str().to_owned()),
            per_page: params.per_page,
        };
        let dashboard_items = self.api.dashboard_items(&query).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas dashboard: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "items": dashboard_items
        })))
    }

    #[tool(
        description = "Read the text of a Canvas attachment without returning the full binary file. Use this for PDF, Word (.docx), and plain-text resources returned by assignment_info or file_info; it avoids oversized base64 resource responses."
    )]
    async fn attachment_text(
        &self,
        Parameters(AttachmentTextParams {
            resource,
            start,
            max_chars,
        }): Parameters<AttachmentTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let attachment = self.api.attachment(&resource).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas attachment: {e}"), None)
        })?;
        let text = attachment_text(&attachment).map_err(|e| ErrorData::invalid_params(e, None))?;
        let total_chars = text.chars().count();
        let start = start.unwrap_or(0);
        if start > total_chars {
            return Err(ErrorData::invalid_params(
                format!("start {start} is beyond the attachment's {total_chars} characters"),
                None,
            ));
        }
        let max_chars = max_chars
            .unwrap_or(DEFAULT_TEXT_PAGE_CHARS)
            .clamp(1, MAX_TEXT_PAGE_CHARS);
        let excerpt = text.chars().skip(start).take(max_chars).collect::<String>();
        let end = start + excerpt.chars().count();
        let next_start = (end < total_chars).then_some(end);
        let images = if attachment.bytes.starts_with(b"%PDF-") {
            Some(pdf_image_info(&attachment.bytes).map_err(|e| {
                ErrorData::internal_error(format!("Failed to inspect Canvas PDF images: {e}"), None)
            })?)
        } else {
            None
        };

        Ok(CallToolResult::structured(serde_json::json!({
            "resource": resource,
            "mime_type": attachment.mime_type,
            "text": excerpt,
            "start": start,
            "end": end,
            "total_chars": total_chars,
            "next_start": next_start,
            "images": images
        })))
    }

    #[tool(
        description = "Read one embedded image from a Canvas PDF attachment. Call attachment_text first to get the available one-based page and image numbers, then use this tool to inspect image-based questions and diagrams."
    )]
    async fn attachment_image(
        &self,
        Parameters(AttachmentImageParams {
            resource,
            page,
            image,
        }): Parameters<AttachmentImageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let attachment = self.api.attachment(&resource).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas attachment: {e}"), None)
        })?;
        let (bytes, info) = pdf_image(&attachment.bytes, page, image)
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let mime_type = info.mime_type.expect("pdf_image always sets a MIME type");
        let metadata = serde_json::to_string(&info).map_err(|e| {
            ErrorData::internal_error(format!("Failed to serialize PDF image metadata: {e}"), None)
        })?;

        Ok(CallToolResult::success(vec![
            ContentBlock::text(metadata),
            ContentBlock::image(BASE64.encode(bytes), mime_type),
        ]))
    }
}

#[tool_handler]
impl ServerHandler for CanvasTool {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let attachment = self.api.attachment(&uri).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas attachment: {e}"), None)
        })?;

        if uri.starts_with("canvas-text://") {
            let mut text =
                attachment_text(&attachment).map_err(|e| ErrorData::invalid_params(e, None))?;
            if text.chars().count() > MAX_TEXT_PAGE_CHARS {
                text = text.chars().take(MAX_TEXT_PAGE_CHARS).collect();
                text.push_str(
                    "\n\n[Text truncated. Use the attachment_text tool with start=15000 to continue.]",
                );
            }
            let contents = ResourceContents::text(text, uri).with_mime_type("text/plain");
            return Ok(ReadResourceResult::new(vec![contents]).into());
        }

        if attachment.bytes.len() > MAX_INLINE_ATTACHMENT_BYTES {
            return Err(ErrorData::invalid_params(
                format!(
                    "Canvas attachment is {} bytes and too large for one inline MCP resource response. Use the attachment_text tool, or the attachment's canvas-text:// resource, instead.",
                    attachment.bytes.len()
                ),
                None,
            ));
        }

        let mut contents = ResourceContents::blob(BASE64.encode(attachment.bytes), uri);
        if let Some(mime_type) = attachment.mime_type {
            contents = contents.with_mime_type(mime_type);
        }

        Ok(ReadResourceResult::new(vec![contents]).into())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if matches!(std::env::args().nth(1).as_deref(), Some("login" | "auth")) {
        return api::interactive_login().await;
    }

    let service = CanvasTool {
        api: api::CanvasApi::new().await?,
    }
    .serve(stdio())
    .await?;
    service.waiting().await?;
    Ok(())
}
