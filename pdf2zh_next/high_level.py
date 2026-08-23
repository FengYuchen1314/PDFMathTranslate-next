import asyncio
import logging
import logging.handlers
import multiprocessing
import multiprocessing.connection
import multiprocessing.queues
import os
import queue
import signal
import sys
import threading
import time
import traceback
from collections.abc import AsyncGenerator
from contextlib import aclosing
from functools import partial
from logging.handlers import QueueHandler
from pathlib import Path

from babeldoc.format.pdf.high_level import async_translate as babeldoc_translate
from babeldoc.format.pdf.translation_config import TranslationConfig as BabelDOCConfig
from babeldoc.format.pdf.translation_config import (
    WatermarkOutputMode as BabelDOCWatermarkMode,
)
from babeldoc.glossary import Glossary
from babeldoc.main import create_progress_handler
from rich.logging import RichHandler

from pdf2zh_next.config.model import SettingsModel
from pdf2zh_next.translator import get_term_translator
from pdf2zh_next.translator import get_translator


# Custom exception classes for structured error handling
class TranslationError(Exception):
    """Base class for all translation-related errors."""

    def __reduce__(self):
        """Support for pickling the exception when passing between processes."""
        return self.__class__, (str(self),)


class BabeldocError(TranslationError):
    """Error originating from the babeldoc library."""

    def __init__(self, message, original_error=None):
        super().__init__(message)
        self.original_error = original_error

    def __reduce__(self):
        """Support for pickling the exception when passing between processes."""
        return self.__class__, (str(self), self.original_error)

    def __str__(self):
        if self.original_error:
            return f"{super().__str__()} - Original error: {self.original_error}"
        return super().__str__()


class SubprocessError(TranslationError):
    """Error occurring in the translation subprocess outside of babeldoc."""

    def __init__(self, message, traceback_str=None):
        self.raw_message = message
        super().__init__(message)
        self.traceback_str = traceback_str

    def __reduce__(self):
        """Support for pickling the exception when passing between processes."""
        return (self.__class__, (self.raw_message, self.traceback_str))

    def __str__(self):
        if self.traceback_str:
            return f"{super().__str__()}\nTraceback: {self.traceback_str}"
        return super().__str__()


class IPCError(TranslationError):
    """Error in inter-process communication."""

    def __init__(self, message, details=None):
        super().__init__(message)
        self.details = details

    def __reduce__(self):
        """Support for pickling the exception when passing between processes."""
        return self.__class__, (str(self), self.details)

    def __str__(self):
        if self.details:
            return f"{super().__str__()} - Details: {self.details}"
        return super().__str__()


class SubprocessCrashError(TranslationError):
    """Error occurring when the subprocess crashes unexpectedly."""

    def __init__(self, message, exit_code=None):
        super().__init__(message)
        self.exit_code = exit_code

    def __reduce__(self):
        """Support for pickling the exception when passing between processes."""
        return self.__class__, (str(self), self.exit_code)

    def __str__(self):
        if self.exit_code is not None:
            return f"{super().__str__()} (exit code: {self.exit_code})"
        return super().__str__()


logger = logging.getLogger(__name__)


def _translate_wrapper(
    settings: SettingsModel,
    file: Path,
    pipe_progress_send: multiprocessing.connection.Connection,
    pipe_cancel_message_recv: multiprocessing.connection.Connection,
    logger_queue: multiprocessing.Queue,
    process_group_id,
):
    # A dedicated process group lets the parent remove BabelDOC descendants
    # (font/PDF helper processes) when cooperative cancellation times out.
    if os.name == "posix":
        try:
            os.setsid()
            process_group_id.value = os.getpgrp()
        except OSError:
            pass
    logger = logging.getLogger(__name__)
    cancel_event = threading.Event()
    queue_handler = None
    try:
        logging.getLogger("asyncio").setLevel(logging.WARNING)
        logging.getLogger("httpx").setLevel(logging.WARNING)
        logging.getLogger("openai").setLevel(logging.WARNING)
        logging.getLogger("pdfminer").setLevel(logging.WARNING)
        logging.getLogger("httpcore").setLevel(logging.WARNING)
        logging.getLogger("peewee").setLevel(logging.WARNING)

        queue_handler = QueueHandler(logger_queue)
        logging.basicConfig(level=logging.INFO, handlers=[queue_handler])

        # Workaround for loky spawning powershell.exe on Windows.
        # loky queries physical CPU cores via powershell.exe without
        # CREATE_NO_WINDOW. Under pythonw.exe (PyStand), there is no
        # parent console, so Windows creates visible console windows.
        # Multiple ThreadPoolExecutor threads hit this simultaneously
        # (cache race), producing many popups. Pre-filling the cache
        # prevents the subprocess call entirely.
        import sys
        if sys.platform == "win32":
            try:
                import joblib.externals.loky.backend.context as _loky_ctx
                if _loky_ctx.physical_cores_cache is None:
                    _loky_ctx.physical_cores_cache = os.cpu_count()
                    logger.info(
                        "Pre-filled loky physical_cores_cache=%d to prevent "
                        "PowerShell window popups under pythonw.exe",
                        os.cpu_count(),
                    )
            except Exception:
                logger.debug(
                    "Failed to pre-fill loky physical_cores_cache",
                    exc_info=True,
                )

        config = create_babeldoc_config(settings, file)

        def cancel_recv_thread():
            try:
                pipe_cancel_message_recv.recv()
                logger.debug("Cancel signal received in subprocess")
                cancel_event.set()
                config.cancel_translation()
            except (EOFError, OSError):
                return
            except Exception as e:
                logger.error(f"Error in cancel_recv_thread: {e}")

        cancel_t = threading.Thread(target=cancel_recv_thread, daemon=True)
        cancel_t.start()

        async def translate_wrapper_async():
            try:
                async for event in babeldoc_translate(config):
                    logger.debug(f"sub process generate event: {event}")
                    if event["type"] == "error":
                        # Convert babeldoc error to structured exception
                        error_msg = str(event.get("error", "Unknown babeldoc error"))
                        error = BabeldocError(
                            message=f"Babeldoc translation error: {error_msg}",
                            original_error=error_msg,
                        )
                        pipe_progress_send.send(error)
                        break
                    # Send normal progress events as before
                    if event["type"] == "finish":
                        # Extract token usage
                        token_usage = {}

                        # Main translator
                        if hasattr(config.translator, "token_count"):
                            token_usage["main"] = {
                                "total": config.translator.token_count.value
                                if hasattr(config.translator, "token_count")
                                else 0,
                                "prompt": config.translator.prompt_token_count.value
                                if hasattr(config.translator, "prompt_token_count")
                                else 0,
                                "completion": config.translator.completion_token_count.value
                                if hasattr(config.translator, "completion_token_count")
                                else 0,
                                "cache_hit_prompt": config.translator.cache_hit_prompt_token_count.value
                                if hasattr(
                                    config.translator, "cache_hit_prompt_token_count"
                                )
                                else 0,
                            }

                        # Term extraction translator
                        if (
                            hasattr(config.term_extraction_translator, "token_count")
                            and config.term_extraction_translator != config.translator
                        ):
                            token_usage["term"] = {
                                "total": config.term_extraction_translator.token_count.value
                                if hasattr(
                                    config.term_extraction_translator, "token_count"
                                )
                                else 0,
                                "prompt": config.term_extraction_translator.prompt_token_count.value
                                if hasattr(
                                    config.term_extraction_translator,
                                    "prompt_token_count",
                                )
                                else 0,
                                "completion": config.term_extraction_translator.completion_token_count.value
                                if hasattr(
                                    config.term_extraction_translator,
                                    "completion_token_count",
                                )
                                else 0,
                                "cache_hit_prompt": config.term_extraction_translator.cache_hit_prompt_token_count.value
                                if hasattr(
                                    config.term_extraction_translator,
                                    "cache_hit_prompt_token_count",
                                )
                                else 0,
                            }
                        elif config.term_extraction_token_usage:
                            token_usage["term"] = {
                                "total": config.term_extraction_token_usage[
                                    "total_tokens"
                                ],
                                "prompt": config.term_extraction_token_usage[
                                    "prompt_tokens"
                                ],
                                "completion": config.term_extraction_token_usage[
                                    "completion_tokens"
                                ],
                                "cache_hit_prompt": config.term_extraction_token_usage[
                                    "cache_hit_prompt_tokens"
                                ],
                            }
                            if sum(token_usage["term"].values()) == 0:
                                token_usage.pop("term")
                        if (
                            "main" in token_usage
                            and "term" in token_usage
                            and config.term_extraction_translator
                            and config.term_extraction_translator == config.translator
                        ):
                            # 如果术语翻译器和主翻译器是同一个实例，避免重复计算
                            term_usage = token_usage["term"]
                            main_usage = token_usage["main"]
                            main_usage["total"] -= term_usage["total"]
                            main_usage["prompt"] -= term_usage["prompt"]
                            main_usage["completion"] -= term_usage["completion"]
                            main_usage["cache_hit_prompt"] -= term_usage[
                                "cache_hit_prompt"
                            ]

                        event["token_usage"] = token_usage
                        pipe_progress_send.send(event)
                        break
                    pipe_progress_send.send(event)
            except Exception as e:
                # Capture non-babeldoc errors during translation
                tb_str = traceback.format_exc()
                if not cancel_event.is_set():
                    logger.error(f"Error in translate_wrapper_async: {e}\n{tb_str}")
                error = SubprocessError(
                    message=f"Error during translation process: {e}",
                    traceback_str=tb_str,
                )
                try:
                    pipe_progress_send.send(error)
                except Exception as pipe_err:
                    if not cancel_event.is_set():
                        logger.error(f"Failed to send error through pipe: {pipe_err}")

        # Run the async translation in the subprocess's event loop
        try:
            asyncio.run(translate_wrapper_async())
        except Exception as e:
            # Capture errors that might occur outside the async context
            tb_str = traceback.format_exc()
            if not cancel_event.is_set():
                logger.error(f"Error running async translation: {e}\n{tb_str}")
            error = SubprocessError(
                message=f"Failed to run translation process: {e}", traceback_str=tb_str
            )
            try:
                pipe_progress_send.send(error)
            except Exception as pipe_err:
                if not cancel_event.is_set():
                    logger.error(f"Failed to send error through pipe: {pipe_err}")
    except Exception as e:
        # Capture any errors during setup or initialization
        tb_str = traceback.format_exc()
        logger.error(f"Subprocess initialization error: {e}\n{tb_str}")
        try:
            error = SubprocessError(
                message=f"Translation subprocess initialization error: {e}",
                traceback_str=tb_str,
            )
            pipe_progress_send.send(error)
        except Exception as pipe_err:
            if not cancel_event.is_set():
                logger.error(f"Failed to send error through pipe: {pipe_err}")
    finally:
        logger.debug("sub process send close")
        try:
            pipe_progress_send.send(None)
            pipe_progress_send.close()
            logger.debug("sub process close pipe progress send")
        except Exception as e:
            if not cancel_event.is_set():
                logger.error(f"Error closing progress pipe: {e}")

        try:
            if queue_handler is not None:
                logging.getLogger().removeHandler(queue_handler)
            logging.getLogger().addHandler(RichHandler())
            logger_queue.put(None)
            logger_queue.close()
        except Exception as e:
            if not cancel_event.is_set():
                logger.error(f"Error closing logger queue: {e}")


def _signal_process_tree(
    translate_process: multiprocessing.Process,
    *,
    force: bool,
    process_group_id: int | None = None,
) -> None:
    """Signal a worker and, on POSIX, every descendant in its private group."""
    if translate_process.pid is None:
        return
    if os.name == "posix":
        try:
            process_group = process_group_id or os.getpgid(translate_process.pid)
            # Never signal a group unless the worker is its leader.  This guard
            # prevents an early-cancel race from touching the server's group.
            if process_group == translate_process.pid:
                os.killpg(process_group, signal.SIGKILL if force else signal.SIGTERM)
                return
        except (OSError, ProcessLookupError):
            pass
    try:
        if force:
            translate_process.kill()
        else:
            translate_process.terminate()
    except (OSError, ProcessLookupError):
        pass


def _process_group_is_alive(process_group_id: int | None) -> bool:
    if os.name != "posix" or not process_group_id:
        return False
    try:
        os.killpg(process_group_id, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def _wait_for_process_group_exit(
    process_group_id: int | None, timeout: float
) -> bool:
    deadline = time.monotonic() + timeout
    while _process_group_is_alive(process_group_id):
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.05)
    return True


async def _translate_in_subprocess(
    settings: SettingsModel,
    file: Path,
):
    """Translate in a spawn worker and reclaim it before publishing ``finish``."""
    ctx = multiprocessing.get_context("spawn")
    loop = asyncio.get_running_loop()
    progress_messages: asyncio.Queue = asyncio.Queue()
    progress_pipe_closed = object()
    receiver_stop = threading.Event()
    log_stop = threading.Event()

    pipe_progress_recv = None
    pipe_progress_send = None
    pipe_cancel_message_recv = None
    pipe_cancel_message_send = None
    logger_queue = None
    translate_process = None
    process_group_id = None
    recv_t = None
    log_t = None
    process_started = False
    finish_event = None
    finish_received_at = None
    terminal_error: TranslationError | None = None
    cleanup_error: TranslationError | None = None
    expected_stop = False
    exit_code = None

    def post_progress(message) -> bool:
        try:
            loop.call_soon_threadsafe(progress_messages.put_nowait, message)
            return True
        except RuntimeError:
            return False

    def recv_thread() -> None:
        while not receiver_stop.is_set():
            try:
                message = pipe_progress_recv.recv()
            except EOFError:
                post_progress(progress_pipe_closed)
                break
            except (OSError, ValueError) as exc:
                if not receiver_stop.is_set():
                    post_progress(IPCError(f"IPC error: {exc}", details=str(exc)))
                break
            except Exception as exc:
                if not receiver_stop.is_set():
                    post_progress(IPCError(f"IPC error: {exc}", details=str(exc)))
                break
            if not post_progress(message) or message is None:
                break

    def log_thread() -> None:
        while not log_stop.is_set():
            try:
                record = logger_queue.get(timeout=0.2)
            except queue.Empty:
                continue
            except (EOFError, OSError, ValueError):
                break
            except Exception:
                logger.exception("Failure in translation log listener")
                break
            if record is None:
                break
            try:
                logger.handle(record)
            except Exception:
                logger.exception("Failure handling a translation log record")
                break

    try:
        pipe_progress_recv, pipe_progress_send = ctx.Pipe(duplex=False)
        pipe_cancel_message_recv, pipe_cancel_message_send = ctx.Pipe(duplex=False)
        logger_queue = ctx.Queue()
        process_group_id = ctx.Value("q", 0)
        translate_process = ctx.Process(
            target=_translate_wrapper,
            args=(
                settings,
                file,
                pipe_progress_send,
                pipe_cancel_message_recv,
                logger_queue,
                process_group_id,
            ),
        )
        translate_process.start()
        process_started = True

        # The parent must close every child-owned endpoint.  Keeping the
        # progress writer open makes EOF impossible after a worker crash.
        pipe_progress_send.close()
        pipe_progress_send = None
        pipe_cancel_message_recv.close()
        pipe_cancel_message_recv = None

        recv_t = threading.Thread(
            target=recv_thread, name="pdf2zh-progress-listener", daemon=True
        )
        recv_t.start()
        log_t = threading.Thread(
            target=log_thread, name="pdf2zh-log-listener", daemon=True
        )
        log_t.start()

        worker_dead_at = None
        last_message_at = time.monotonic()
        while True:
            try:
                message = await asyncio.wait_for(progress_messages.get(), timeout=0.2)
                last_message_at = time.monotonic()
            except TimeoutError:
                if (
                    finish_received_at is not None
                    and time.monotonic() - finish_received_at >= 10
                ):
                    logger.warning(
                        "Translation worker did not close its IPC channel after finish"
                    )
                    break
                if time.monotonic() - last_message_at > 30 * 60:
                    terminal_error = IPCError(
                        "Translation subprocess produced no event for 30 minutes"
                    )
                    break
                if not translate_process.is_alive():
                    worker_dead_at = worker_dead_at or time.monotonic()
                    # Give the receiver a bounded chance to drain messages that
                    # were written immediately before the process sentinel.
                    if time.monotonic() - worker_dead_at >= 1.0:
                        if finish_event is None and terminal_error is None:
                            code = translate_process.exitcode
                            if code not in (0, None):
                                terminal_error = SubprocessCrashError(
                                    "Translation subprocess exited without a final event",
                                    exit_code=code,
                                )
                            else:
                                terminal_error = IPCError(
                                    "Translation subprocess exited without a finish event"
                                )
                        break
                else:
                    worker_dead_at = None
                continue

            if message is progress_pipe_closed or message is None:
                if finish_event is None and terminal_error is None:
                    code = translate_process.exitcode
                    if code not in (0, None):
                        terminal_error = SubprocessCrashError(
                            "Translation subprocess closed its IPC channel unexpectedly",
                            exit_code=code,
                        )
                    else:
                        terminal_error = IPCError(
                            "Translation subprocess closed without a finish event"
                        )
                break
            if isinstance(message, TranslationError):
                terminal_error = message
                break
            if not isinstance(message, dict):
                terminal_error = IPCError(
                    f"Unexpected message type from subprocess: {type(message)}"
                )
                break
            if message.get("type") == "finish":
                finish_event = message
                finish_received_at = time.monotonic()
                # The child follows with an explicit pipe sentinel.  Do not
                # expose finish until the finally block has joined the worker.
                continue
            if finish_event is not None:
                terminal_error = IPCError("Received progress after the finish event")
                break
            yield message
    except asyncio.CancelledError:
        expected_stop = True
        logger.info("Process Translation cancelled")
        raise
    except GeneratorExit:
        expected_stop = True
        raise
    except KeyboardInterrupt:
        expected_stop = True
        logger.info("KeyboardInterrupt received in main process")
        raise
    finally:
        active_exception = sys.exc_info()[0] is not None

        # Cooperative cancellation first.  It invokes BabelDOC's own cancel
        # hook and normally releases caches and temporary files cleanly.
        if process_started and translate_process.is_alive() and finish_event is None:
            try:
                pipe_cancel_message_send.send(True)
            except (OSError, BrokenPipeError, EOFError):
                pass
        if pipe_cancel_message_send is not None:
            try:
                pipe_cancel_message_send.close()
            except (OSError, ValueError):
                pass
            pipe_cancel_message_send = None

        if process_started:
            try:
                translate_process.join(timeout=5 if finish_event is not None else 3)
                worker_group = (
                    int(process_group_id.value) if process_group_id is not None else 0
                )
                if translate_process.is_alive() or _process_group_is_alive(worker_group):
                    logger.warning(
                        "Translation worker tree did not stop; terminating its group"
                    )
                    _signal_process_tree(
                        translate_process,
                        force=False,
                        process_group_id=worker_group,
                    )
                    translate_process.join(timeout=2)
                    _wait_for_process_group_exit(worker_group, timeout=2)
                # The leader often exits before a TERM-resistant descendant.
                # Check the cached process group independently of leader state.
                if translate_process.is_alive() or _process_group_is_alive(worker_group):
                    logger.warning("Translation worker group still alive; killing it")
                    _signal_process_tree(
                        translate_process,
                        force=True,
                        process_group_id=worker_group,
                    )
                    translate_process.join(timeout=1)
                    _wait_for_process_group_exit(worker_group, timeout=1)
                if translate_process.is_alive() or _process_group_is_alive(worker_group):
                    cleanup_error = SubprocessCrashError(
                        "Could not stop translation subprocess tree", exit_code=None
                    )
                exit_code = translate_process.exitcode
            except Exception:
                logger.exception("Failed while stopping translation subprocess")
                if not active_exception:
                    cleanup_error = IPCError("Failed to stop translation subprocess")

        # A dead worker closes its writer and wakes the receiver.  Only after a
        # bounded join do we close the reader from this thread.
        receiver_stop.set()
        if recv_t is not None:
            recv_t.join(timeout=1)
        if pipe_progress_recv is not None:
            try:
                pipe_progress_recv.close()
            except (OSError, ValueError):
                pass
            pipe_progress_recv = None
        if recv_t is not None and recv_t.is_alive():
            recv_t.join(timeout=0.2)
            if recv_t.is_alive():
                logger.warning("Progress listener did not stop within the cleanup bound")

        # The child normally sends the log sentinel.  If it was killed, the
        # stop event releases get(timeout=...) without the parent ever writing
        # into a possibly broken multiprocessing queue.
        log_stop.set()
        if log_t is not None:
            log_t.join(timeout=1)
            if log_t.is_alive():
                logger.warning("Log listener did not stop within the cleanup bound")
        if logger_queue is not None:
            try:
                logger_queue.cancel_join_thread()
            except (AttributeError, OSError, ValueError):
                pass
            try:
                logger_queue.close()
            except (OSError, ValueError):
                pass
            logger_queue = None

        for connection in (pipe_progress_send, pipe_cancel_message_recv):
            if connection is not None:
                try:
                    connection.close()
                except (OSError, ValueError):
                    pass

        if translate_process is not None:
            try:
                translate_process.close()
            except ValueError:
                logger.warning("Could not close a still-running translation process")

        logger.debug("Translation worker exit code: %s", exit_code)

        # Cleanup must never replace cancellation, GeneratorExit, timeout, or
        # another exception already propagating from the consumer.
        if active_exception or expected_stop:
            cleanup_error = None

    if cleanup_error is not None:
        raise cleanup_error
    if terminal_error is not None:
        raise terminal_error
    if finish_event is None:
        if exit_code not in (0, None):
            raise SubprocessCrashError(
                "Translation subprocess exited without a finish event",
                exit_code=exit_code,
            )
        raise IPCError("Translation subprocess exited without a finish event")

    # This is deliberately the final suspension point: all process, pipe and
    # queue cleanup above has completed before callers observe success.
    yield finish_event


def _get_glossaries(settings: SettingsModel) -> list[Glossary] | None:
    glossaries = []
    if not settings.translation.glossaries:
        return None
    for file in settings.translation.glossaries.split(","):
        glossaries.append(
            Glossary.from_csv(Path(file), target_lang_out=settings.translation.lang_out)
        )
    return glossaries


def create_babeldoc_config(settings: SettingsModel, file: Path) -> BabelDOCConfig:
    if not isinstance(settings, SettingsModel):
        raise ValueError(f"{type(settings)} is not SettingsModel")
    translator = get_translator(settings)
    if translator is None:
        raise ValueError("No translator found")

    if (
        settings.term_extraction_engine_settings == settings.translate_engine_settings
        and settings.translation.term_qps == settings.translation.qps
    ):
        term_extraction_translator = translator
        if recommended_qps := getattr(translator, "pdf2zh_next_recommended_qps", None):
            settings.translation.term_qps = recommended_qps
            logger.info(f"Updated term qps to {recommended_qps}")
        if recommended_pool_max_workers := getattr(
            translator, "pdf2zh_next_recommended_pool_max_workers", None
        ):
            settings.translation.term_pool_max_workers = recommended_pool_max_workers
            logger.info(
                f"Updated term pool max workers to {recommended_pool_max_workers}"
            )
    else:
        term_extraction_translator = get_term_translator(settings)

    # 设置分割策略
    split_strategy = None
    if settings.pdf.max_pages_per_part:
        split_strategy = BabelDOCConfig.create_max_pages_per_part_split_strategy(
            settings.pdf.max_pages_per_part
        )

    # 设置水印模式
    watermark_output_mode_maps = {
        "no_watermark": BabelDOCWatermarkMode.NoWatermark,
        "both": BabelDOCWatermarkMode.Both,
        "watermarked": BabelDOCWatermarkMode.Watermarked,
    }

    watermark_output_mode = settings.pdf.watermark_output_mode

    watermark_mode = watermark_output_mode_maps.get(
        watermark_output_mode, BabelDOCWatermarkMode.Watermarked
    )

    table_model = None
    if settings.pdf.translate_table_text:
        from babeldoc.docvision.table_detection.rapidocr import RapidOCRModel

        table_model = RapidOCRModel()

    babeldoc_config = BabelDOCConfig(
        input_file=file,
        font=None,
        pages=settings.pdf.pages,
        output_dir=settings.translation.output,
        doc_layout_model=None,
        translator=translator,
        debug=settings.basic.debug,
        lang_in=settings.translation.lang_in,
        lang_out=settings.translation.lang_out,
        no_dual=settings.pdf.no_dual,
        no_mono=settings.pdf.no_mono,
        qps=settings.translation.qps,
        # 传递原来缺失的参数
        formular_font_pattern=settings.pdf.formular_font_pattern,
        formular_char_pattern=settings.pdf.formular_char_pattern,
        split_short_lines=settings.pdf.split_short_lines,
        short_line_split_factor=settings.pdf.short_line_split_factor,
        disable_rich_text_translate=settings.pdf.disable_rich_text_translate,
        dual_translate_first=settings.pdf.dual_translate_first,
        enhance_compatibility=settings.pdf.enhance_compatibility,
        use_alternating_pages_dual=settings.pdf.use_alternating_pages_dual,
        watermark_output_mode=watermark_mode,
        min_text_length=settings.translation.min_text_length,
        report_interval=settings.report_interval,
        skip_clean=settings.pdf.skip_clean,
        # 添加分割策略
        split_strategy=split_strategy,
        # 添加表格模型，仅在需要翻译表格时
        table_model=table_model,
        skip_scanned_detection=settings.pdf.skip_scanned_detection,
        ocr_workaround=settings.pdf.ocr_workaround,
        custom_system_prompt=settings.translation.custom_system_prompt,
        glossaries=_get_glossaries(settings),
        auto_enable_ocr_workaround=settings.pdf.auto_enable_ocr_workaround,
        pool_max_workers=settings.translation.pool_max_workers,
        auto_extract_glossary=not settings.translation.no_auto_extract_glossary,
        primary_font_family=settings.translation.primary_font_family,
        only_include_translated_page=settings.pdf.only_include_translated_page,
        # BabelDOC v0.5.1 new options
        merge_alternating_line_numbers=not settings.pdf.no_merge_alternating_line_numbers,
        remove_non_formula_lines=not settings.pdf.no_remove_non_formula_lines,
        non_formula_line_iou_threshold=settings.pdf.non_formula_line_iou_threshold,
        figure_table_protection_threshold=settings.pdf.figure_table_protection_threshold,
        skip_formula_offset_calculation=settings.pdf.skip_formula_offset_calculation,
        # Term extraction translator (can be different from main translator)
        term_extraction_translator=term_extraction_translator,
        term_pool_max_workers=settings.translation.term_pool_max_workers,
    )
    return babeldoc_config


async def do_translate_async_stream(
    settings: SettingsModel, file: Path | str
) -> AsyncGenerator[dict, None]:
    settings.validate_settings()
    if isinstance(file, str):
        file = Path(file)

    if settings.basic.input_files and len(settings.basic.input_files):
        logger.warning(
            "settings.basic.input_files is for cli & config, "
            "pdf2zh_next.highlevel.do_translate_async_stream will ignore this field "
            "and only translate the file pointed to by the file parameter."
        )

    if not file.exists():
        raise FileNotFoundError(f"file {file} not found")

    # 开始翻译
    translate_func = partial(_translate_in_subprocess, settings, file)

    if settings.basic.debug:
        babeldoc_config = create_babeldoc_config(settings, file)
        logger.debug("debug mode, translate in main process")
        translate_func = partial(babeldoc_translate, translation_config=babeldoc_config)
    else:
        logger.info("translate in subprocess")

    try:
        async with aclosing(translate_func()) as event_stream:
            async for event in event_stream:
                yield event
                if settings.basic.debug:
                    logger.debug(event)
                if event["type"] == "finish":
                    break
    except TranslationError as e:
        # Log and re-raise structured errors
        logger.error(f"Translation error: {e}")
        if isinstance(e, BabeldocError) and e.original_error:
            logger.error(f"Original babeldoc error: {e.original_error}")
        elif isinstance(e, SubprocessError) and e.traceback_str:
            logger.error(f"Subprocess traceback: {e.traceback_str}")
        # Create an error event to yield to client code
        error_event = {
            "type": "error",
            "error": str(e) if not isinstance(e, SubprocessError) else e.raw_message,
            "error_type": e.__class__.__name__,
            "details": getattr(e, "original_error", "")
            or getattr(e, "traceback_str", "")
            or "",
        }
        yield error_event
        raise  # Re-raise the exception so that the caller can handle it if needed


async def do_translate_file_async(
    settings: SettingsModel, ignore_error: bool = False
) -> int:
    rich_pbar_config = BabelDOCConfig(
        translator=None,
        lang_in=None,
        lang_out=None,
        input_file=None,
        font=None,
        pages=None,
        output_dir=None,
        doc_layout_model=1,
        use_rich_pbar=True,
    )
    progress_context, progress_handler = create_progress_handler(rich_pbar_config)
    input_files = settings.basic.input_files
    assert len(input_files) >= 1, "At least one input file is required"
    settings.basic.input_files = set()

    error_count = 0

    for file in input_files:
        logger.info(f"translate file: {file}")
        # 开始翻译
        with progress_context:
            try:
                async for event in do_translate_async_stream(settings, file):
                    progress_handler(event)
                    if settings.basic.debug:
                        logger.debug(event)
                    if event["type"] == "finish":
                        result = event["translate_result"]
                        logger.info("Translation Result:")
                        logger.info(f"  Original PDF: {result.original_pdf_path}")
                        logger.info(f"  Time Cost: {result.total_seconds:.2f}s")
                        logger.info(f"  Mono PDF: {result.mono_pdf_path or 'None'}")
                        logger.info(f"  Dual PDF: {result.dual_pdf_path or 'None'}")

                        token_usage = event.get("token_usage", {})
                        if token_usage:
                            logger.info("Token Usage:")
                            total_usage = {
                                "total": 0,
                                "prompt": 0,
                                "cache_hit_prompt": 0,
                                "completion": 0,
                            }
                            if "main" in token_usage:
                                main_usage = token_usage["main"]
                                logger.info(
                                    f"  Main Translator: Total {main_usage['total']}, Prompt {main_usage['prompt']}, Cache Hit Prompt {main_usage['cache_hit_prompt']}, Completion {main_usage['completion']}"
                                )
                                total_usage["total"] += main_usage["total"]
                                total_usage["prompt"] += main_usage["prompt"]
                                total_usage["cache_hit_prompt"] += main_usage[
                                    "cache_hit_prompt"
                                ]
                                total_usage["completion"] += main_usage["completion"]
                            if "term" in token_usage:
                                term_usage = token_usage["term"]
                                logger.info(
                                    f"  Term Translator: Total {term_usage['total']}, Prompt {term_usage['prompt']}, Cache Hit Prompt {term_usage['cache_hit_prompt']}, Completion {term_usage['completion']}"
                                )
                                total_usage["total"] += term_usage["total"]
                                total_usage["prompt"] += term_usage["prompt"]
                                total_usage["cache_hit_prompt"] += term_usage[
                                    "cache_hit_prompt"
                                ]
                                total_usage["completion"] += term_usage["completion"]
                            logger.info(
                                f"  Total Token Usage: Total {total_usage['total']}, Prompt {total_usage['prompt']}, Cache Hit Prompt {total_usage['cache_hit_prompt']}, Completion {total_usage['completion']}"
                            )
                        break
                    if event["type"] == "error":
                        error_msg = event.get("error", "Unknown error")
                        error_type = event.get("error_type", "UnknownError")
                        details = event.get("details", "")

                        logger.error(f"Error translating file {file}: {error_msg}")
                        logger.error(f"Error type: {error_type}")
                        if details:
                            logger.error(f"Error details: {details}")

                        error_count += 1
                        if not ignore_error:
                            raise RuntimeError(f"Translation error: {error_msg}")
                        break
            except TranslationError as e:
                # Already logged in do_translate_async_stream
                error_count += 1
                if not ignore_error:
                    raise
            except Exception as e:
                logger.error(f"Error translating file {file}: {e}")
                error_count += 1
                if not ignore_error:
                    raise

    return error_count


def do_translate_file(settings: SettingsModel, ignore_error: bool = False) -> int:
    """
    Translate files synchronously, returning the number of errors encountered.

    Args:
        settings: Translation settings
        ignore_error: If True, continue translating other files when an error occurs

    Returns:
        Number of errors encountered during translation

    Raises:
        TranslationError: If a translation error occurs and ignore_error is False
        Exception: For other errors if ignore_error is False
    """
    try:
        return asyncio.run(do_translate_file_async(settings, ignore_error))
    except KeyboardInterrupt:
        logger.info("Translation interrupted by user (Ctrl+C)")
        return 1  # Return error count = 1 to indicate interruption
    except RuntimeError as e:
        # Handle the case where run() is called from a running event loop
        if "asyncio.run() cannot be called from a running event loop" in str(e):
            loop = asyncio.get_event_loop()
            try:
                return loop.run_until_complete(
                    do_translate_file_async(settings, ignore_error)
                )
            except KeyboardInterrupt:
                logger.info("Translation interrupted by user (Ctrl+C) in event loop")
                return 1  # Return error count = 1 to indicate interruption
        else:
            raise
