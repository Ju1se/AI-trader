from pathlib import Path

from alembic import command
from alembic.config import Config
from sqlalchemy import create_engine, inspect

from app.db import Base


def test_migrations_build_the_same_schema_as_the_models(tmp_path):
    """Guards against changing a model without writing the migration for it."""
    url = f"sqlite:///{tmp_path / 'migrated.db'}"
    config = Config(str(Path(__file__).resolve().parent.parent / "alembic.ini"))
    config.set_main_option("sqlalchemy.url", url)
    command.upgrade(config, "head")

    inspector = inspect(create_engine(url))
    migrated = {table: {c["name"] for c in inspector.get_columns(table)}
                for table in inspector.get_table_names() if table != "alembic_version"}
    expected = {table.name: {c.name for c in table.columns} for table in Base.metadata.sorted_tables}
    assert migrated == expected
