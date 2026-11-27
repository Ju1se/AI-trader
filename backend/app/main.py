import asyncio
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import FastAPI, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse

from . import sim
from .api import dev_router, router
from .config import settings
from .db import SessionLocal
from .events import bus
from .services import ApiError

DESCRIPTION = """
Agent 工作流程的控制面:任务状态机、批准、动作记录、审计和成本台账。

- 身份:开发期用 `X-User-Id` 请求头占位。
- 错误:业务错误统一返回 `{"error": {"code", "message"}}`。
- 时间:一律是带 `Z` 的 UTC ISO 8601 字符串。
- `/v1/dev/*` 只用于联调,代替还没接入的 Agent。

接口的用法和联调脚本见仓库里的 `docs/api.md`。
"""


def run_migrations() -> None:
    from alembic import command
    from alembic.config import Config

    config = Config(str(Path(__file__).resolve().parent.parent / "alembic.ini"))
    config.set_main_option("sqlalchemy.url", settings.database_url)
    command.upgrade(config, "head")


@asynccontextmanager
async def lifespan(app: FastAPI):
    if settings.auto_migrate:
        run_migrations()
    bus.bind(asyncio.get_running_loop())
    if settings.seed_on_start:
        with SessionLocal() as db:
            sim.load(db, reset=True)
    yield


def create_app() -> FastAPI:
    app = FastAPI(title="AI-trader 控制面 API", version="0.1.0", description=DESCRIPTION, lifespan=lifespan)
    app.add_middleware(CORSMiddleware, allow_origins=settings.cors_origins, allow_methods=["*"],
                       allow_headers=["*"])

    @app.exception_handler(ApiError)
    async def api_error(request: Request, exc: ApiError) -> JSONResponse:
        return JSONResponse(status_code=exc.status, content={"error": {"code": exc.code, "message": exc.message}})

    app.include_router(router)
    if settings.enable_dev_endpoints:
        app.include_router(dev_router)
    return app


app = create_app()
