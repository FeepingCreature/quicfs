# QuicFS HTTP/3 API Specification

All paths must be valid UTF-8. All responses include a `status` field indicating success/failure.
Errors return appropriate HTTP status codes and include an `error` field in the JSON response.

## File Operations

### Read File
GET `/fs/file/read`
- Query params: 
  - `path`: UTF-8 file path
  - `offset`: byte offset (integer)
  - `length`: number of bytes to read
- Success: 200 OK with raw bytes
- Errors:
  - 404: File not found
  - 400: Invalid offset/length
  - 403: Permission denied

### Write File
PUT `/fs/file/write`
- Query params:
  - `path`: UTF-8 file path
  - `offset`: byte offset (integer)
- Body: raw bytes to write
- Success: 200 OK
- Errors:
  - 404: Parent directory not found
  - 403: Permission denied
  - 507: Insufficient storage

### Create File
POST `/fs/file/create`
- Query params:
  - `path`: UTF-8 file path
  - `mode`: Unix permissions (optional, default 0644)
- Success: 201 Created
- Errors:
  - 409: File already exists
  - 404: Parent directory not found
  - 403: Permission denied

### Delete File
DELETE `/fs/file`
- Query params:
  - `path`: UTF-8 file path
- Success: 200 OK
- Errors:
  - 404: File not found
  - 403: Permission denied

### Truncate File
POST `/fs/file/truncate`
- Query params:
  - `path`: UTF-8 file path
  - `size`: new size in bytes
- Success: 200 OK
- Errors:
  - 404: File not found
  - 403: Permission denied
  - 507: Insufficient storage

## Directory Operations

### Create Directory
POST `/fs/dir/create`
- Query params:
  - `path`: UTF-8 directory path
  - `mode`: Unix permissions (optional, default 0755)
- Success: 201 Created
- Errors:
  - 409: Directory already exists
  - 404: Parent directory not found
  - 403: Permission denied

### Remove Directory
DELETE `/fs/dir`
- Query params:
  - `path`: UTF-8 directory path
- Success: 200 OK
- Errors:
  - 404: Directory not found
  - 403: Permission denied
  - 409: Directory not empty

### List Directory
GET `/fs/dir/list`
- Query params:
  - `path`: UTF-8 directory path
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
GET `/fs/attr`
- Query params:
  - `path`: UTF-8 file path
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
PATCH `/fs/attr`
- Query params:
  - `path`: UTF-8 file path
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
POST `/fs/rename`
- Query params:
  - `from`: UTF-8 source path
  - `to`: UTF-8 destination path
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
