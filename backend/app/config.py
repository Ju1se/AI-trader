from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_file=".env", env_prefix="AITRADER_", extra="ignore")

    database_url: str = "sqlite:///./dev.db"
    # Dev endpoints let the frontend drive a task through every state without an agent.
    enable_dev_endpoints: bool = True
    seed_on_start: bool = False
    # Apply Alembic migrations at startup. Turn off where migrations run as a deploy step.
    auto_migrate: bool = True
    cors_origins: list[str] = ["http://localhost:3000"]
    approval_ttl_hours: int = 24
    input_ttl_hours: int = 48


settings = Settings()
