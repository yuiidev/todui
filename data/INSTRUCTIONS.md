# TODO Data Instructions

This directory contains one JSON file per project. Each `*.json` file is loaded by the TUI as a project, so every project file must be valid JSON and must match the shape below. Do not include comments or trailing commas in JSON files.

## Writing Content

Each TODO should be Human-first oriented, this means:
- A Human needs to be able to glean all required information to complete the TODO without relying on current LLM session context.
- The TODO should not contain large continuous difficult to visually parse swaths of text.
- When providing mappings or any other sort of information to could be concisely represented as a table prefer that over pure text.
- Do not put absolute path references to anything in the TODO unless it is as part of a argument to a cli tool, instead only filename + extension + line number (if relevant); example: (example-file.txt:64)

## Example Project

````json
{
  "title": "example-project",
  "description": "Short summary of the project and its current todo focus. Keep this in sync with the tasks.",
  "labels": [
    "example-project"
  ],
  "tasks": [
    {
      "id": "0197f4f6-5e70-7c7d-9e7b-66b93df52a64",
      "title": "Replace placeholder implementation",
      "content": "Explain the task in enough detail that a future agent can continue without rediscovery. Include context, reasoning, decisions already made, and any constraints.\n\nUse `inline code` for identifiers and commands. Use fenced code blocks when useful:\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```",
      "labels": [
        "rust",
        "cleanup"
      ],
      "branch": "",
      "created_at": "2026-06-25T10:10:06+02:00",
      "updated_at": null,
      "completed_at": null,
      "due_at": null
    }
  ]
}
````

## Project Fields

- `title`: Required string. Use the project or repository name.
- `description`: Required string. Keep it short and update it when the task list changes meaningfully.
- `labels`: Required array of strings. Use stable, lowercase labels where practical.
- `tasks`: Required array of task objects.

## Task Fields

- `id`: Required string. Use a UUIDv7 and never change it after creation.
- `title`: Required string. Write a short imperative title, for example `Replace ext-intl with Symfony Intl`.
- `content`: Required string. Describe the context, reasoning, decisions made, constraints, and expected outcome. Newlines are supported as JSON string escapes such as `\n`.
- `labels`: Required array of strings. Use labels for topic, area, urgency, or tool context.
- `branch`: Required value. Use `null` when the branch is `main` or irrelevant to the TODO.
- `created_at`: Required RFC3339 datetime string with timezone. Set this when creating the task and never change it.
- `updated_at`: Required value. Use `null` for a new task. Set an RFC3339 datetime when changing an existing task.
- `completed_at`: Required value. Use `null` while open. Set an RFC3339 datetime when completed.
- `due_at`: Required value. Use `null` when there is no due date, otherwise use an RFC3339 datetime.

## Content Styling

The TUI supports a small Markdown-like subset in `content`:

- `_text_` renders as italic.
- `**text**` renders as bold.
- `` `text` `` renders as inline code.
- Triple-backtick fenced code blocks render as code blocks.
- Fenced code blocks may include a language tag, for example:
  ````
  ```rust
  ```
  ````
  or
  ````
  ```c
  ```
  ````
  The TUI will syntax highlight supported languages.
- Markdown tables

Avoid relying on other Markdown features. They may remain plain text.

## Update Rules

- When adding a task, append a complete task object with a new UUIDv7 `id`.
- When editing an existing task, preserve `id` and `created_at`.
- When completing a task, set `completed_at` and also update `updated_at`. Completed tasks older than two weeks are hidden in the default TUI task list.
- When reopening a completed task, set `completed_at` back to `null` and update `updated_at`.
- When changing project scope or adding/removing meaningful tasks, update the project `description`.
