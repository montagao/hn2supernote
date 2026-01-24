"""Command-line interface for supernote_uploader."""

from __future__ import annotations

import contextlib
import json
import sys
from pathlib import Path

import click

from supernote_uploader import (
    AuthenticationError,
    FileInfo,
    FolderInfo,
    SupernoteClient,
    VerificationRequiredError,
)

# Default token cache location
DEFAULT_TOKEN_CACHE = Path.home() / ".supernote" / "token_cache.json"


def _load_last_email(cache_path: Path) -> str | None:
    """Load the last used email from the token cache."""
    if not cache_path.exists():
        return None
    try:
        data = json.loads(cache_path.read_text())
        return data.get("_last_email")
    except Exception:
        return None


def _save_last_email(cache_path: Path, email: str) -> None:
    """Save the last used email to the token cache."""
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        data = json.loads(cache_path.read_text()) if cache_path.exists() else {}
        data["_last_email"] = email
        cache_path.write_text(json.dumps(data, indent=2))
    except Exception:
        pass  # Non-critical, ignore errors


def get_client(
    email: str | None = None,
    password: str | None = None,
    token_cache: Path | None = None,
) -> tuple[SupernoteClient, str | None]:
    """Create a SupernoteClient, attempting to use cached credentials.

    Returns (client, email) where email is the resolved email (from cache if not provided).
    """
    cache_path = token_cache or DEFAULT_TOKEN_CACHE
    cache_path.parent.mkdir(parents=True, exist_ok=True)

    # If no email provided, try to get the last used email
    resolved_email = email
    if not resolved_email:
        resolved_email = _load_last_email(cache_path)

    client = SupernoteClient(
        email=resolved_email,
        password=password,
        auto_login=False,  # We'll handle login manually to check cached token
        token_cache_path=cache_path,
    )

    # Try to authenticate with cached token if we have an email
    if resolved_email:
        with contextlib.suppress(AuthenticationError):
            # This will attempt to use cached token
            client.login(resolved_email, password or "")

    return client, resolved_email


@click.group()
@click.version_option(package_name="supernote-uploader")
def main() -> None:
    """Supernote Cloud CLI - Upload files to your Supernote device."""
    pass


@main.command()
@click.option("--email", "-e", prompt=True, help="Supernote account email")
@click.option(
    "--password", "-p", prompt=True, hide_input=True, help="Supernote account password"
)
@click.option(
    "--token-cache",
    type=click.Path(path_type=Path),
    default=None,
    help=f"Token cache file path (default: {DEFAULT_TOKEN_CACHE})",
)
def login(email: str, password: str, token_cache: Path | None) -> None:
    """Login to Supernote Cloud and cache credentials."""
    cache_path = token_cache or DEFAULT_TOKEN_CACHE
    try:
        client = SupernoteClient(
            email=email,
            password=password,
            auto_login=False,
            token_cache_path=cache_path,
        )
        try:
            client.login(email, password)
            _save_last_email(cache_path, email)
            click.echo(click.style("Login successful!", fg="green"))
        except VerificationRequiredError as e:
            click.echo("Email verification required. A code was sent to your email.")
            code = click.prompt("Enter verification code")
            client.verify(code, e.verification_context)
            _save_last_email(cache_path, email)
            click.echo(click.style("Verification successful!", fg="green"))
        finally:
            client.close()
    except AuthenticationError as e:
        click.echo(click.style(f"Login failed: {e}", fg="red"), err=True)
        sys.exit(1)


@main.command()
@click.argument("files", nargs=-1, required=True, type=click.Path(exists=True, path_type=Path))
@click.option(
    "--folder",
    "-f",
    default="/Inbox",
    help="Target folder on Supernote (default: /Inbox)",
)
@click.option("--email", "-e", envvar="SUPERNOTE_EMAIL", help="Supernote account email")
@click.option(
    "--password",
    "-p",
    envvar="SUPERNOTE_PASSWORD",
    help="Supernote account password",
)
@click.option(
    "--token-cache",
    type=click.Path(path_type=Path),
    default=None,
    help="Token cache file path",
)
@click.option(
    "--no-create-folder",
    is_flag=True,
    help="Don't create the target folder if it doesn't exist",
)
def upload(
    files: tuple[Path, ...],
    folder: str,
    email: str | None,
    password: str | None,
    token_cache: Path | None,
    no_create_folder: bool,
) -> None:
    """Upload files to Supernote Cloud.

    FILES: One or more PDF or EPUB files to upload.

    Examples:

        supernote upload article.pdf

        supernote upload *.pdf --folder /Documents/Articles

        supernote upload book.epub -f /Books
    """
    cache_path = token_cache or DEFAULT_TOKEN_CACHE
    try:
        client, resolved_email = get_client(email, password, cache_path)

        # If not authenticated, prompt for credentials
        if not client.is_authenticated:
            if not resolved_email:
                resolved_email = click.prompt("Email")
            if not password:
                password = click.prompt("Password", hide_input=True)
            try:
                client.login(resolved_email, password)
                _save_last_email(cache_path, resolved_email)
            except VerificationRequiredError as e:
                click.echo("Email verification required. A code was sent to your email.")
                code = click.prompt("Enter verification code")
                client.verify(code, e.verification_context)
                _save_last_email(cache_path, resolved_email)

        # Upload files
        results = client.upload_many(
            list(files),
            folder,
            create_folder=not no_create_folder,
        )

        # Print results
        success_count = 0
        for result in results:
            if result.success:
                click.echo(
                    click.style("✓ ", fg="green") + f"{result.file_name} -> {result.cloud_path}"
                )
                success_count += 1
            else:
                click.echo(
                    click.style("✗ ", fg="red") + f"{result.file_name}: {result.error}",
                    err=True,
                )

        # Summary
        total = len(results)
        if success_count == total:
            click.echo(click.style(f"\nAll {total} file(s) uploaded successfully!", fg="green"))
        else:
            click.echo(f"\n{success_count}/{total} file(s) uploaded.", err=True)
            sys.exit(1)

        client.close()

    except AuthenticationError as e:
        click.echo(click.style(f"Authentication failed: {e}", fg="red"), err=True)
        sys.exit(1)
    except Exception as e:
        click.echo(click.style(f"Error: {e}", fg="red"), err=True)
        sys.exit(1)


@main.command("ls")
@click.argument("path", default="/")
@click.option("--email", "-e", envvar="SUPERNOTE_EMAIL", help="Supernote account email")
@click.option(
    "--password",
    "-p",
    envvar="SUPERNOTE_PASSWORD",
    help="Supernote account password",
)
@click.option(
    "--token-cache",
    type=click.Path(path_type=Path),
    default=None,
    help="Token cache file path",
)
def list_folder(
    path: str,
    email: str | None,
    password: str | None,
    token_cache: Path | None,
) -> None:
    """List contents of a folder on Supernote Cloud.

    PATH: Folder path to list (default: /)

    Examples:

        supernote ls

        supernote ls /Inbox

        supernote ls /Documents/Articles
    """
    cache_path = token_cache or DEFAULT_TOKEN_CACHE
    try:
        client, resolved_email = get_client(email, password, cache_path)

        if not client.is_authenticated:
            if not resolved_email:
                resolved_email = click.prompt("Email")
            if not password:
                password = click.prompt("Password", hide_input=True)
            client.login(resolved_email, password)
            _save_last_email(cache_path, resolved_email)

        items = client.list_folder(path)

        if not items:
            click.echo(f"(empty folder: {path})")
        else:
            for item in items:
                if isinstance(item, FolderInfo):
                    click.echo(click.style(f"  {item.name}/", fg="blue"))
                elif isinstance(item, FileInfo):
                    size_str = _format_size(item.size)
                    click.echo(f"  {item.name}  ({size_str})")

        client.close()

    except AuthenticationError as e:
        click.echo(click.style(f"Authentication failed: {e}", fg="red"), err=True)
        sys.exit(1)
    except Exception as e:
        click.echo(click.style(f"Error: {e}", fg="red"), err=True)
        sys.exit(1)


@main.command()
@click.argument("path")
@click.option("--parents", "-p", is_flag=True, help="Create parent directories as needed")
@click.option("--email", "-e", envvar="SUPERNOTE_EMAIL", help="Supernote account email")
@click.option(
    "--password",
    envvar="SUPERNOTE_PASSWORD",
    help="Supernote account password",
)
@click.option(
    "--token-cache",
    type=click.Path(path_type=Path),
    default=None,
    help="Token cache file path",
)
def mkdir(
    path: str,
    parents: bool,
    email: str | None,
    password: str | None,
    token_cache: Path | None,
) -> None:
    """Create a folder on Supernote Cloud.

    PATH: Folder path to create

    Examples:

        supernote mkdir /Inbox/Articles

        supernote mkdir /Documents/Work/Projects --parents
    """
    cache_path = token_cache or DEFAULT_TOKEN_CACHE
    try:
        client, resolved_email = get_client(email, password, cache_path)

        if not client.is_authenticated:
            if not resolved_email:
                resolved_email = click.prompt("Email")
            if not password:
                password = click.prompt("Password", hide_input=True)
            client.login(resolved_email, password)
            _save_last_email(cache_path, resolved_email)

        client.mkdir(path, parents=parents)
        click.echo(click.style(f"Created folder: {path}", fg="green"))

        client.close()

    except AuthenticationError as e:
        click.echo(click.style(f"Authentication failed: {e}", fg="red"), err=True)
        sys.exit(1)
    except Exception as e:
        click.echo(click.style(f"Error: {e}", fg="red"), err=True)
        sys.exit(1)


def _format_size(size_bytes: int) -> str:
    """Format file size in human-readable form."""
    for unit in ["B", "KB", "MB", "GB"]:
        if size_bytes < 1024:
            return f"{size_bytes:.1f} {unit}" if unit != "B" else f"{size_bytes} {unit}"
        size_bytes /= 1024  # type: ignore[assignment]
    return f"{size_bytes:.1f} TB"


if __name__ == "__main__":
    main()
