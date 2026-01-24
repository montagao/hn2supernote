"""Data models for the supernote_uploader library."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class UploadResult:
    """Result of an upload operation."""

    success: bool
    file_path: Path
    cloud_path: str
    file_name: str
    error: str | None = None


@dataclass(frozen=True)
class FileInfo:
    """Information about a file in Supernote cloud."""

    id: int
    name: str
    path: str
    size: int


@dataclass(frozen=True)
class FolderInfo:
    """Information about a folder in Supernote cloud."""

    id: int
    name: str
    path: str
