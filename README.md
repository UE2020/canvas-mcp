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
```

The first time you use it, ask your agent to run the authentication tool. This will open a browser window where you'll have to log into Canvas. Once you're logged in, just close the window.

# Features & Tool Calls

- **Authentication**: `auth` (logs into Canvas via browser session)
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
  - `attachment_text`: Inspect extracted text from PDF, Word (.docx), or text attachments
  - `attachment_image`: Extract embedded images from PDF documents
