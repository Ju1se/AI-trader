"""Database setup.

Portability rules, so the same models run on SQLite now and on cloud PostgreSQL later:
- only portable column types (String, Text, Numeric, JSON, naive-UTC DateTime);
- every constraint and index gets a deterministic name from NAMING, so Alembic
  migrations are identical on both databases;
- JSON columns become JSONB on PostgreSQL and plain JSON elsewhere;
- concurrency relies on a version column (compare-and-set), not on database-specific locks.
"""

from sqlalchemy import JSON, MetaData, create_engine, event
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm import DeclarativeBase, sessionmaker
from sqlalchemy.pool import StaticPool

from .config import settings

NAMING = {
    "ix": "ix_%(column_0_label)s",
    "uq": "uq_%(table_name)s_%(column_0_name)s",
    "ck": "ck_%(table_name)s_%(constraint_name)s",
    "fk": "fk_%(table_name)s_%(column_0_name)s_%(referred_table_name)s",
    "pk": "pk_%(table_name)s",
}

JSONType = JSON().with_variant(JSONB(), "postgresql")


class Base(DeclarativeBase):
    metadata = MetaData(naming_convention=NAMING)


def make_engine(url: str):
    kwargs: dict = {}
    if url.startswith("sqlite"):
        kwargs["connect_args"] = {"check_same_thread": False}
        if url in ("sqlite://", "sqlite:///:memory:"):
            # One shared connection, so an in-memory database survives across sessions.
            kwargs["poolclass"] = StaticPool
    else:
        kwargs["pool_pre_ping"] = True  # cloud databases drop idle connections
    new_engine = create_engine(url, **kwargs)
    if url.startswith("sqlite"):
        # SQLite ignores foreign keys unless asked. Enforce them so it rejects what PostgreSQL would.
        @event.listens_for(new_engine, "connect")
        def _enforce_foreign_keys(dbapi_connection, _record):
            dbapi_connection.execute("PRAGMA foreign_keys=ON")

    return new_engine


engine = make_engine(settings.database_url)
SessionLocal = sessionmaker(bind=engine, expire_on_commit=False)


def get_db():
    db = SessionLocal()
    try:
        yield db
    finally:
        db.close()
