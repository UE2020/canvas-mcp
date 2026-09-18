This repository implements a Canvas MCP server that doesn't require you to generate an API token (e.g. to bypass organization restrictions).

# Setup

Build it using cargo:
```
cargo build --release
```

The binary will be placed inside target/release.

Then, configure your agent to use it. You'll need to specify the base url to use when logging in and accessing the API via an environment variable. For example, using Codex:

```toml
[mcp_servers.canvas]
command = "C:\\path\\to\\canvas-mcp.exe"
args = []

[mcp_servers.canvas.env]
CANVAS_URL = "https://canvas.university.edu"
# Optional: customize inline attachment resource size limit in bytes (defaults to 524288 / 512KB)
# CANVAS_MAX_INLINE_BYTES = "1048576"
# Optional: customize directory where attachments are downloaded (defaults to OS Downloads/Canvas)
# CANVAS_DOWNLOAD_DIR = "C:\\path\\to\\custom\\downloads"
```

The first time you use it, ask your agent to run the authentication tool. This will open a browser window where you'll have to log into Canvas. Once you're logged in, just close the window.

# Features & Tool Calls

- **Authentication**:
  - `refresh_auth`: silently reload and validate cookies from the saved browser profile
  - `auth`: open a visible browser for interactive login when the saved session has expired
- **Courses & Content**:
  - `course_list`: List user's courses with details and grades
  - `course_info`: Structured course details and syllabus
  - `module_list`: List course modules and module items
  - `page_info`: Fetch specific Canvas course page content
  - `file_info`: Fetch file metadata and resource links
- **Assignments**:
  - `assignment_list`: List assignments in a course
  - `assignment_info`: Assignment instructions, submission status, and attachments
- **Grades**:
  - `course_grades`: Read total course grades (current score/grade, final score/grade) and individual assignment grades
  - `assignment_grade`: Read user's grade and submission details for a specific assignment
  - `grade_summary`: Read total grades across all active/enrolled courses
- **Planner & To-Do Items**:
  - `dashboard_items`: Read planner items across date ranges and courses
  - `create_planner_note` / `create_custom_planner_item`: Create a custom planner item / note
  - `update_planner_note` / `update_custom_planner_item`: Edit an existing custom planner note
  - `delete_planner_note` / `delete_custom_planner_item`: Delete a custom planner note
  - `planner_note_info`: Retrieve details of a specific planner note
- **Attachments**:
  - `download_attachment` / `attachment_download`: Download binary or text attachments directly to local disk (defaults to OS `Downloads/Canvas`, supports custom destination paths/directories and Canvas file IDs)
  - `attachment_text`: Inspect extracted text from PDF, Word (.docx), or text attachments
  - `attachment_image`: Extract embedded images from PDF documents
