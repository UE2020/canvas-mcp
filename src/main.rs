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
struct CreatePlannerNoteParams {
    /// Title of the planner note / custom planner item.
    title: Option<String>,
    /// Text content / description of the planner note.
    details: Option<String>,
    /// Scheduled date or timestamp (YYYY-MM-DD or ISO 8601).
    todo_date: Option<String>,
    /// Optional Canvas course ID to associate with the note.
    course_id: Option<String>,
    /// Optional learning object type to link: 'announcement', 'assignment', 'discussion_topic', 'wiki_page', 'quiz'.
    linked_object_type: Option<String>,
    /// Optional learning object ID to link (must be used in conjunction with linked_object_type and course_id).
    linked_object_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct UpdatePlannerNoteParams {
    /// Canvas planner note ID to update.
    note_id: String,
    /// Updated title of the planner note.
    title: Option<String>,
    /// Updated text content / description of the planner note.
    details: Option<String>,
    /// Updated date or timestamp for the note (YYYY-MM-DD or ISO 8601).
    todo_date: Option<String>,
    /// Updated course ID (pass empty string to remove course association).
    course_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DeletePlannerNoteParams {
    /// Canvas planner note ID to delete.
    note_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PlannerNoteInfoParams {
    /// Canvas planner note ID.
    note_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CourseGradesParams {
    /// Canvas course ID.
    course_id: String,
    /// Optional Canvas assignment ID. If provided, returns only the grade for that specific assignment along with the course total grade. If omitted, returns all assignment grades.
    assignment_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AssignmentGradeParams {
    /// Canvas course ID.
    course_id: String,
    /// Canvas assignment ID.
    assignment_id: String,
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
    color_space: Option<String>,
    bits_per_component: Option<i64>,
    filters: Vec<String>,
    mime_type: Option<&'static str>,
    supported: bool,
    unsupported_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DownloadAttachmentParams {
    /// Canvas attachment resource URI (e.g. 'canvas://files/...', 'canvas-text://files/...'), relative path, or Canvas download URL. Either resource or file_id must be provided.
    resource: Option<String>,
    /// Canvas file ID (can be provided instead of resource).
    file_id: Option<String>,
    /// Destination file or directory path. If omitted, downloads to the current working directory using the attachment's filename. If a directory path is given (or ends with a slash), the file is saved inside that directory.
    destination_path: Option<String>,
    /// Optional filename override. If omitted, uses the filename from Canvas or Content-Disposition.
    filename: Option<String>,
}

const DEFAULT_MAX_INLINE_ATTACHMENT_BYTES: usize = 512 * 1024;
const CANVAS_MAX_INLINE_BYTES_ENV: &str = "CANVAS_MAX_INLINE_BYTES";

fn max_inline_attachment_bytes() -> usize {
    std::env::var(CANVAS_MAX_INLINE_BYTES_ENV)
        .ok()
        .and_then(|val| val.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_INLINE_ATTACHMENT_BYTES)
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

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
    match filters {
        [filter] if filter == "DCTDecode" => Some("image/jpeg"),
        [filter] if filter == "JPXDecode" => Some("image/jp2"),
        _ => None,
    }
}

fn raw_image_color_type(
    color_space: Option<&str>,
    bits_per_component: Option<i64>,
) -> Result<png::ColorType, String> {
    if bits_per_component != Some(8) {
        return Err(format!(
            "unsupported bits per component {:?}; lossless PDF images currently require 8",
            bits_per_component
        ));
    }

    match color_space {
        Some("DeviceGray" | "G") => Ok(png::ColorType::Grayscale),
        Some("DeviceRGB" | "RGB") => Ok(png::ColorType::Rgb),
        Some(value) => Err(format!("unsupported color space {value:?}")),
        None => Err("image has no directly supported color space".to_owned()),
    }
}

fn raw_image_support(
    filters: &[String],
    color_space: Option<&str>,
    bits_per_component: Option<i64>,
) -> Result<png::ColorType, String> {
    if !filters.iter().all(|filter| {
        matches!(
            filter.as_str(),
            "FlateDecode" | "LZWDecode" | "ASCII85Decode"
        )
    }) {
        return Err(format!("unsupported filter chain {filters:?}"));
    }
    raw_image_color_type(color_space, bits_per_component)
}

fn pdf_image_support(
    filters: &[String],
    color_space: Option<&str>,
    bits_per_component: Option<i64>,
) -> Result<&'static str, String> {
    if let Some(mime_type) = image_mime_type(filters) {
        Ok(mime_type)
    } else {
        raw_image_support(filters, color_space, bits_per_component).map(|_| "image/png")
    }
}

fn encode_png(
    pixels: &[u8],
    width: i64,
    height: i64,
    color_type: png::ColorType,
) -> Result<Vec<u8>, String> {
    let width = u32::try_from(width).map_err(|_| format!("invalid image width {width}"))?;
    let height = u32::try_from(height).map_err(|_| format!("invalid image height {height}"))?;
    if width == 0 || height == 0 {
        return Err(format!("invalid image dimensions {width}x{height}"));
    }
    let channels = match color_type {
        png::ColorType::Grayscale => 1_u64,
        png::ColorType::Rgb => 3,
        _ => return Err("unsupported PNG color type".to_owned()),
    };
    let expected_len = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|value| value.checked_mul(channels))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("image dimensions {width}x{height} are too large"))?;
    if pixels.len() != expected_len {
        return Err(format!(
            "decoded image has {} bytes; expected {expected_len} for {width}x{height} {color_type:?}",
            pixels.len()
        ));
    }

    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color_type);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("failed to create PNG: {e}"))?;
        writer
            .write_image_data(pixels)
            .map_err(|e| format!("failed to encode PNG pixels: {e}"))?;
    }
    Ok(bytes)
}

fn pdf_image_info(bytes: &[u8]) -> Result<Vec<PdfImageInfo>, String> {
    let document =
        lopdf::Document::load_mem(bytes).map_err(|e| format!("failed to parse PDF images: {e}"))?;
    let mut info = Vec::new();
    for (page, page_id) in document.get_pages() {
        let images = document.get_page_images(page_id).unwrap_or_default();
        for (index, image) in images.iter().enumerate() {
            let filters = image.filters.clone().unwrap_or_default();
            let support = pdf_image_support(
                &filters,
                image.color_space.as_deref(),
                image.bits_per_component,
            );
            info.push(PdfImageInfo {
                page,
                image: index + 1,
                width: image.width,
                height: image.height,
                color_space: image.color_space.clone(),
                bits_per_component: image.bits_per_component,
                mime_type: support.as_ref().ok().copied(),
                supported: support.is_ok(),
                unsupported_reason: support.err(),
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
    let mime_type = pdf_image_support(
        &filters,
        image.color_space.as_deref(),
        image.bits_per_component,
    )
    .map_err(|reason| format!("PDF page {page} image {index} is unsupported: {reason}"))?;
    let bytes = if mime_type == "image/png" {
        let stream = document
            .get_object(image.id)
            .and_then(lopdf::Object::as_stream)
            .map_err(|e| format!("failed to read PDF page {page} image {index} stream: {e}"))?;
        let pixels = stream
            .decompressed_content()
            .map_err(|e| format!("failed to decompress PDF page {page} image {index}: {e}"))?;
        let color_type = raw_image_support(
            &filters,
            image.color_space.as_deref(),
            image.bits_per_component,
        )?;
        encode_png(&pixels, image.width, image.height, color_type)?
    } else {
        image.content.to_vec()
    };
    let info = PdfImageInfo {
        page,
        image: index,
        width: image.width,
        height: image.height,
        color_space: image.color_space.clone(),
        bits_per_component: image.bits_per_component,
        filters,
        mime_type: Some(mime_type),
        supported: true,
        unsupported_reason: None,
    };
    Ok((bytes, info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use lopdf::{Dictionary, Document, Object, Stream, dictionary};
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
            filename: None,
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
            filename: None,
        };

        assert_eq!(attachment_text(&attachment).unwrap(), "problem one");
    }

    fn image_pdf(
        color_space: &str,
        bits_per_component: i64,
        width: i64,
        height: i64,
        content: Vec<u8>,
        decode_params: Option<Dictionary>,
    ) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let mut image_dictionary = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => width,
            "Height" => height,
            "ColorSpace" => Object::Name(color_space.as_bytes().to_vec()),
            "BitsPerComponent" => bits_per_component,
            "Filter" => "FlateDecode",
        };
        if let Some(decode_params) = decode_params {
            image_dictionary.set("DecodeParms", decode_params);
        }
        let image_id = document.add_object(Stream::new(image_dictionary, content));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), width.into(), height.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im1" => image_id },
            },
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        pdf
    }

    fn zlib_compress(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn decode_png(bytes: &[u8]) -> (png::OutputInfo, Vec<u8>) {
        let decoder = png::Decoder::new(Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels).unwrap();
        pixels.truncate(info.buffer_size());
        (info, pixels)
    }

    #[test]
    fn extracts_flate_rgb_image_as_png() {
        let pixels = vec![255, 0, 0, 0, 255, 0];
        let pdf = image_pdf("DeviceRGB", 8, 2, 1, zlib_compress(&pixels), None);

        let (bytes, info) = pdf_image(&pdf, 1, 1).unwrap();
        let (png_info, decoded_pixels) = decode_png(&bytes);

        assert_eq!(info.mime_type, Some("image/png"));
        assert!(info.supported);
        assert_eq!(png_info.width, 2);
        assert_eq!(png_info.height, 1);
        assert_eq!(png_info.color_type, png::ColorType::Rgb);
        assert_eq!(decoded_pixels, pixels);
    }

    #[test]
    fn extracts_flate_grayscale_image_with_png_predictor() {
        // PNG predictor rows include a leading filter byte. Zero means no row filter.
        let predicted = [0, 0, 127, 255];
        let decode_params = dictionary! {
            "Predictor" => 15,
            "Colors" => 1,
            "BitsPerComponent" => 8,
            "Columns" => 3,
        };
        let pdf = image_pdf(
            "DeviceGray",
            8,
            3,
            1,
            zlib_compress(&predicted),
            Some(decode_params),
        );

        let (bytes, info) = pdf_image(&pdf, 1, 1).unwrap();
        let (png_info, decoded_pixels) = decode_png(&bytes);

        assert_eq!(info.mime_type, Some("image/png"));
        assert_eq!(png_info.color_type, png::ColorType::Grayscale);
        assert_eq!(decoded_pixels, [0, 127, 255]);
    }

    #[test]
    fn reports_unsupported_lossless_image_metadata() {
        let pixels = vec![0; 4];
        let pdf = image_pdf("DeviceCMYK", 8, 1, 1, zlib_compress(&pixels), None);

        let info = pdf_image_info(&pdf).unwrap().remove(0);

        assert!(!info.supported);
        assert_eq!(info.mime_type, None);
        assert_eq!(info.color_space.as_deref(), Some("DeviceCMYK"));
        assert!(
            info.unsupported_reason
                .unwrap()
                .contains("unsupported color space")
        );
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
            filename: None,
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
            filename: None,
        };

        assert!(
            attachment_text(&attachment)
                .unwrap_err()
                .contains("failed to parse Word document XML")
        );
    }

    #[test]
    fn honors_max_inline_bytes_env() {
        assert_eq!(
            max_inline_attachment_bytes(),
            DEFAULT_MAX_INLINE_ATTACHMENT_BYTES
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
        description = "Authenticate the Canvas connector with the user's institutional login. Use this tool whenever another Canvas tool reports that cookies are missing, the session expired, or Canvas redirected to a login page. Before calling it, inform the user that the Canvas session is not authenticated and that a visible browser will open; ask them to finish signing in and close the browser window when done. The connector refreshes its cached cookies afterward and closes the browser."
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
        description = "Get a specific assignment through the read-only Canvas REST API. Pass the Canvas course ID and assignment ID. Returns structured dates, grading and submission settings, HTML instructions, all attachments (use download_attachment to save them to disk), and the current user's submission status when available."
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
        description = "Get Canvas file metadata through the read-only Files REST API. Pass the file content_id returned by module_list. Returns names, type, size, timestamps, availability, and canvas-text:// and canvas:// resources for text extraction or downloading to disk via download_attachment."
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
        description = "Read one embedded image from a Canvas PDF attachment. Call attachment_text first and select an image whose metadata has supported=true, then pass its one-based page and image numbers to inspect image-based questions and diagrams."
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

    #[tool(
        description = "Download a Canvas attachment or file directly to local disk. Pass either 'resource' (the 'canvas://...' download_resource or 'canvas-text://...' resource from assignment_info or file_info) or 'file_id'. You can optionally specify 'destination_path' (a file path or directory) and 'filename' override. Returns the absolute saved path, filename, byte size, and MIME type."
    )]
    async fn download_attachment(
        &self,
        Parameters(params): Parameters<DownloadAttachmentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let downloaded = if let Some(ref file_id) = params.file_id {
            self.api
                .download_file(
                    file_id,
                    params.destination_path.as_deref(),
                    params.filename.as_deref(),
                )
                .await
        } else if let Some(ref resource) = params.resource {
            if resource.chars().all(|c| c.is_ascii_digit()) && !resource.is_empty() {
                self.api
                    .download_file(
                        resource,
                        params.destination_path.as_deref(),
                        params.filename.as_deref(),
                    )
                    .await
            } else {
                self.api
                    .download_attachment(
                        resource,
                        params.destination_path.as_deref(),
                        params.filename.as_deref(),
                    )
                    .await
            }
        } else {
            return Err(ErrorData::invalid_params(
                "either 'resource' or 'file_id' must be provided",
                None,
            ));
        }
        .map_err(|e| {
            ErrorData::internal_error(format!("Failed to download Canvas attachment: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "saved_path": downloaded.saved_path,
            "filename": downloaded.filename,
            "bytes": downloaded.bytes,
            "mime_type": downloaded.mime_type
        })))
    }

    #[tool(
        description = "Download a Canvas attachment or file directly to local disk. Alias for download_attachment."
    )]
    async fn attachment_download(
        &self,
        params: Parameters<DownloadAttachmentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.download_attachment(params).await
    }

    #[tool(
        description = "Create a custom planner item (Canvas planner note) through the Canvas REST API (POST /api/v1/planner_notes). Allows the agent to write custom to-do items and notes to self for a course or general schedule. Accepts title, details (note body text), todo_date (YYYY-MM-DD or ISO 8601), course_id, and optional linked learning object."
    )]
    async fn create_planner_note(
        &self,
        Parameters(params): Parameters<CreatePlannerNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload = api::CreatePlannerNotePayload {
            title: params.title,
            details: params.details,
            todo_date: params.todo_date,
            course_id: params.course_id,
            linked_object_type: params.linked_object_type,
            linked_object_id: params.linked_object_id,
        };
        let note = self.api.create_planner_note(&payload).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to create Canvas planner note: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "note": note
        })))
    }

    #[tool(description = "Create a custom planner item in Canvas. Alias for create_planner_note.")]
    async fn create_custom_planner_item(
        &self,
        params: Parameters<CreatePlannerNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.create_planner_note(params).await
    }

    #[tool(
        description = "Edit an existing custom planner item (Canvas planner note) through the Canvas REST API (PUT /api/v1/planner_notes/:id). Pass note_id and any fields to update (title, details, todo_date, course_id)."
    )]
    async fn update_planner_note(
        &self,
        Parameters(params): Parameters<UpdatePlannerNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload = api::UpdatePlannerNotePayload {
            title: params.title,
            details: params.details,
            todo_date: params.todo_date,
            course_id: params.course_id,
        };
        let note = self
            .api
            .update_planner_note(&params.note_id, &payload)
            .await
            .map_err(|e| {
                ErrorData::internal_error(
                    format!("Failed to update Canvas planner note: {e}"),
                    None,
                )
            })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "note": note
        })))
    }

    #[tool(
        description = "Edit an existing custom planner item in Canvas. Alias for update_planner_note."
    )]
    async fn update_custom_planner_item(
        &self,
        params: Parameters<UpdatePlannerNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.update_planner_note(params).await
    }

    #[tool(
        description = "Delete a custom planner item (Canvas planner note) through the Canvas REST API (DELETE /api/v1/planner_notes/:id). Permanently removes the note from the user's planner."
    )]
    async fn delete_planner_note(
        &self,
        Parameters(DeletePlannerNoteParams { note_id }): Parameters<DeletePlannerNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self.api.delete_planner_note(&note_id).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to delete Canvas planner note: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "deleted": true,
            "note_id": note_id,
            "result": result
        })))
    }

    #[tool(description = "Delete a custom planner item in Canvas. Alias for delete_planner_note.")]
    async fn delete_custom_planner_item(
        &self,
        params: Parameters<DeletePlannerNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.delete_planner_note(params).await
    }

    #[tool(
        description = "Get details of a specific custom planner item (Canvas planner note) through the read-only Canvas REST API (GET /api/v1/planner_notes/:id)."
    )]
    async fn planner_note_info(
        &self,
        Parameters(PlannerNoteInfoParams { note_id }): Parameters<PlannerNoteInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let note = self.api.planner_note_info(&note_id).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch Canvas planner note: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "note": note
        })))
    }

    #[tool(
        description = "Read your grades for a Canvas course through the read-only Canvas REST API. Returns your total course grade (including current score percentage, current letter grade, final score percentage, and final letter grade) and your grade on each assignment (or a specific assignment if assignment_id is supplied), including score received, points possible, submission status (graded, submitted, unsubmitted, missing, late, excused), and timestamps."
    )]
    async fn course_grades(
        &self,
        Parameters(CourseGradesParams {
            course_id,
            assignment_id,
        }): Parameters<CourseGradesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let res = self
            .api
            .course_grades(&course_id, assignment_id.as_deref())
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to fetch course grades: {e}"), None)
            })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "course_grades": res
        })))
    }

    #[tool(
        description = "Read your grade for a specific assignment in a course through the read-only Canvas REST API. Returns the assignment grade details (score received, letter grade, points possible, submission status, due date, timestamps) along with the course's total grade."
    )]
    async fn assignment_grade(
        &self,
        Parameters(AssignmentGradeParams {
            course_id,
            assignment_id,
        }): Parameters<AssignmentGradeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let res = self
            .api
            .course_grades(&course_id, Some(&assignment_id))
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to fetch assignment grade: {e}"), None)
            })?;

        let assignment = res.assignments.into_iter().next();
        Ok(CallToolResult::structured(serde_json::json!({
            "course_id": res.course_id,
            "course_name": res.course_name,
            "total_grade": res.total_grade,
            "assignment_grade": assignment
        })))
    }

    #[tool(
        description = "Read your total grades across all enrolled Canvas courses through the read-only Canvas REST API. Returns a summary of each course with current score percentage, current letter grade, final score percentage, final letter grade, and enrollment state."
    )]
    async fn grade_summary(&self) -> Result<CallToolResult, ErrorData> {
        let res = self.api.grade_summary().await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch grade summary: {e}"), None)
        })?;

        Ok(CallToolResult::structured(serde_json::json!({
            "grades": res
        })))
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

        let max_inline_bytes = max_inline_attachment_bytes();
        if attachment.bytes.len() > max_inline_bytes {
            let downloaded = self
                .api
                .save_attachment_to_disk(
                    &uri,
                    &attachment.bytes,
                    attachment.filename.as_deref(),
                    attachment.mime_type.as_deref(),
                    None,
                )
                .await
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!("Failed to auto-download Canvas attachment: {e}"),
                        None,
                    )
                })?;

            let text = format!(
                "Canvas attachment ({size_str}) exceeds the inline MCP limit ({limit_str}).\n\
                 It was automatically downloaded to your local workspace:\n  \
                 {saved_path}\n\n\
                 Filename: {filename}\n\
                 Bytes: {bytes}\n\
                 MIME type: {mime_type}\n\n\
                 You can inspect, read, or extract this file directly on your local system.",
                size_str = format_bytes(downloaded.bytes),
                limit_str = format_bytes(max_inline_bytes as u64),
                saved_path = downloaded.saved_path,
                filename = downloaded.filename,
                bytes = downloaded.bytes,
                mime_type = downloaded
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
            );
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type("text/plain")
            ])
            .into());
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
