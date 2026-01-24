"""Shared test helpers for supernote_uploader tests."""

from __future__ import annotations


class MockFileItem:
    """Mock sncloud file/folder item."""

    def __init__(
        self,
        id: int,
        file_name: str,
        is_folder: bool = False,
        file_size: int = 0,
    ) -> None:
        self.id = id
        self.file_name = file_name
        self.is_folder = is_folder
        self.file_size = file_size
