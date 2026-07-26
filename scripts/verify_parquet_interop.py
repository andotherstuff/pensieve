# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "duckdb==1.5.5",
#   "pyarrow==25.0.0",
# ]
# ///
"""Verify the checked-in canonical V1 fixture with two independent readers."""

from pathlib import Path
import sys

import duckdb
import pyarrow as pa
import pyarrow.parquet as pq


ROOT = Path(__file__).resolve().parents[1]
FIXTURE = (
    ROOT
    / "crates"
    / "pensieve-parquet"
    / "tests"
    / "fixtures"
    / "valid-v1.parquet"
)
VERSION_KEY = b"nostr.event_archive.version"


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def verify_pyarrow() -> list[dict[str, object]]:
    parquet_file = pq.ParquetFile(FIXTURE)
    expected_schema = pa.schema(
        [
            pa.field("id", pa.binary(32), nullable=False),
            pa.field("pubkey", pa.binary(32), nullable=False),
            pa.field("created_at", pa.uint64(), nullable=False),
            pa.field("kind", pa.uint16(), nullable=False),
            pa.field(
                "tags",
                pa.list_(
                    pa.field(
                        "element",
                        pa.list_(
                            pa.field("element", pa.string(), nullable=False)
                        ),
                        nullable=False,
                    )
                ),
                nullable=False,
            ),
            pa.field("content", pa.string(), nullable=False),
            pa.field("sig", pa.binary(64), nullable=False),
        ],
        metadata={VERSION_KEY: b"1"},
    )
    check(
        parquet_file.schema_arrow == expected_schema,
        f"PyArrow schema mismatch:\n{parquet_file.schema_arrow}",
    )
    check(parquet_file.metadata.num_rows == 3, "PyArrow row count mismatch")
    check(parquet_file.metadata.num_row_groups == 1, "PyArrow row-group mismatch")
    check(
        parquet_file.metadata.metadata == {VERSION_KEY: b"1"},
        "PyArrow footer metadata mismatch",
    )

    created_at_stats = parquet_file.metadata.row_group(0).column(2).statistics
    check(created_at_stats is not None, "PyArrow did not expose created_at statistics")
    check(created_at_stats.null_count == 0, "created_at null_count mismatch")
    check(created_at_stats.min == 0, "created_at minimum mismatch")
    check(
        created_at_stats.max == 2**63,
        "created_at unsigned maximum was narrowed or corrupted",
    )

    rows = pq.read_table(FIXTURE).to_pylist()
    check(rows[0]["tags"] == [], "empty tags were not preserved")
    check(rows[0]["content"] == "", "empty content was not preserved")
    check(rows[1]["content"] == " \n\t", "whitespace-only content was not preserved")
    check(rows[2]["kind"] == 65535, "u16 maximum kind was not preserved")
    check(rows[2]["created_at"] == 2**63, "u64 timestamp was not preserved")
    check(rows[2]["tags"][0] == ["alt"], "one-element tag was not preserved")
    check(rows[2]["tags"][1] == ["d", ""], "empty tag string was not preserved")
    check(
        rows[2]["content"] == '  \nUnicode: 🦀\n{"exact":true}',
        "Unicode or multiline content changed",
    )
    return rows


def verify_duckdb(pyarrow_rows: list[dict[str, object]]) -> None:
    connection = duckdb.connect()
    description = connection.execute(
        "DESCRIBE SELECT * FROM read_parquet(?)", [str(FIXTURE)]
    ).fetchall()
    expected_names_and_types = [
        ("id", "BLOB"),
        ("pubkey", "BLOB"),
        ("created_at", "UBIGINT"),
        ("kind", "USMALLINT"),
        ("tags", "VARCHAR[][]"),
        ("content", "VARCHAR"),
        ("sig", "BLOB"),
    ]
    check(
        [(row[0], row[1]) for row in description] == expected_names_and_types,
        f"DuckDB schema mismatch: {description}",
    )

    rows = connection.execute(
        """
        SELECT id, pubkey, created_at, kind, tags, content, sig
        FROM read_parquet(?)
        ORDER BY created_at, id
        """,
        [str(FIXTURE)],
    ).fetchall()
    expected_rows = [
        (
            row["id"],
            row["pubkey"],
            row["created_at"],
            row["kind"],
            row["tags"],
            row["content"],
            row["sig"],
        )
        for row in pyarrow_rows
    ]
    check(rows == expected_rows, "DuckDB and PyArrow decoded different logical rows")

    high_timestamp_rows = connection.execute(
        "SELECT count(*) FROM read_parquet(?) WHERE created_at >= ?",
        [str(FIXTURE), 2**63],
    ).fetchone()[0]
    check(
        high_timestamp_rows == 1,
        "DuckDB did not query the unsigned created_at boundary correctly",
    )


def main() -> int:
    pyarrow_rows = verify_pyarrow()
    verify_duckdb(pyarrow_rows)
    print(
        "interoperability verified: "
        f"PyArrow {pa.__version__}, DuckDB {duckdb.__version__}, "
        f"{len(pyarrow_rows)} canonical rows"
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"interoperability verification failed: {error}", file=sys.stderr)
        raise
