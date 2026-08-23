from unittest.mock import Mock

from pdf2zh_next.translator.translator_impl.google import GOOGLE_REQUEST_TIMEOUT
from pdf2zh_next.translator.translator_impl.google import GoogleTranslator


def test_google_requests_have_a_finite_timeout():
    translator = object.__new__(GoogleTranslator)
    translator.session = Mock()
    translator.endpoint = "https://translate.google.com/m"
    translator.headers = {"User-Agent": "test"}
    translator.lang_in = "en"
    translator.lang_out = "zh-CN"

    response = Mock(status_code=200, text='<div class="result-container">你好</div>')
    translator.session.get.return_value = response

    assert translator.do_translate("hello") == "你好"
    translator.session.get.assert_called_once_with(
        translator.endpoint,
        params={"tl": "zh-CN", "sl": "en", "q": "hello"},
        headers=translator.headers,
        timeout=GOOGLE_REQUEST_TIMEOUT,
    )
