# QuicFS HTTP/3 API Specification

All paths must be valid UTF-8.
Errors return appropriate HTTP status codes and include an `error` field in the JSON response.

## File Operations

### Read File
GET `/file/*path`
- Headers:
  - `Range: bytes=start-end` (optional)
- Success: 
  - 200 OK with complete file content
  - 206 Partial Content with requested range
  - Headers:
    - `Content-Range: bytes start-end/total`
    - `Accept-Ranges: bytes`
- Errors:
  - 404: File not found
  - 400: Invalid offset/length
  - 403: Permission denied

### Write File Range
PATCH `/file/*path`
- Headers:
  - `Content-Range: bytes start-end/total`
- Body: raw bytes to write
- Success: 200 OK
- Errors:
  - 404: Parent directory not found
  - 403: Permission denied
  - 507: Insufficient storage

### Create File
PUT `/file/*path`
- Query params:
  - `path`: UTF-8 file path
  - `mode`: Unix permissions (optional, default 0644)
- Success: 201 Created
- Errors:
  - 409: File already exists
  - 404: Parent directory not found
  - 403: Permission denied

### Delete File
DELETE `/file/*path`
- Success: 200 OK
- Errors:
  - 404: File not found
  - 403: Permission denied

### Truncate File
PATCH `/file/*path`
- Headers:
  - `Content-Length: 0`
  - `Content-Range: bytes */new_size`
- Success: 200 OK
- Errors:
  - 404: File not found
  - 403: Permission denied
  - 507: Insufficient storage

## Directory Operations

### Create Directory
PUT `/dir/*path`
- Query params:
  - `mode`: Unix permissions (optional, default 0755)
- Success: 201 Created
- Errors:
  - 409: Directory already exists
  - 404: Parent directory not found
  - 403: Permission denied

### Remove Directory
DELETE `/dir/*path`
- Success: 200 OK
- Errors:
  - 404: Directory not found
  - 403: Permission denied
  - 409: Directory not empty

### List Directory
GET `/dir/*path`
- Success: 200 OK with JSON:
  ```json
  {
    "entries": [
      {
        "name": "filename",
        "type": "file|dir",
        "size": 1234,
        "mode": 0644,
        "mtime": "2024-02-18T15:04:05Z",
        "atime": "2024-02-18T15:04:05Z",
        "ctime": "2024-02-18T15:04:05Z"
      }
    ]
  }
  ```
- Errors:
  - 404: Directory not found
  - 403: Permission denied

## Metadata Operations

### Get Attributes
GET `/attr/*path`
- Success: 200 OK with JSON:
  ```json
  {
    "type": "file|dir",
    "size": 1234,
    "mode": 0644,
    "mtime": "2024-02-18T15:04:05Z",
    "atime": "2024-02-18T15:04:05Z",
    "ctime": "2024-02-18T15:04:05Z"
  }
  ```
- Errors:
  - 404: Path not found
  - 403: Permission denied

### Set Attributes
PATCH `/attr/*path`
- Body: JSON with attributes to change:
  ```json
  {
    "mode": 0644,
    "mtime": "2024-02-18T15:04:05Z",
    "atime": "2024-02-18T15:04:05Z"
  }
  ```
- Success: 200 OK
- Errors:
  - 404: Path not found
  - 403: Permission denied

### Rename/Move
PATCH `/file/*path`
- Headers:
  - `Destination: /file/new/path`
- Success: 200 OK
- Errors:
  - 404: Source not found
  - 409: Destination exists
  - 403: Permission denied

## Filesystem Info

### Get FS Stats
GET `/fs/stats`
- Success: 200 OK with JSON:
  ```json
  {
    "total_space": 1234567,
    "available_space": 1234567,
    "files": 1234,
    "files_free": 1234
  }
  ```
- Errors:
  - 403: Permission denied

## Error Response Format
```json
{
  "status": "error",
  "error": "Error message",
  "code": "ERROR_CODE",
  "details": {}
}
```

Common error codes:
- ENOENT: Path not found
- EACCES: Permission denied
- EEXIST: Path already exists
- ENOTEMPTY: Directory not empty
- ENOSPC: No space left
- EINVAL: Invalid arguments
