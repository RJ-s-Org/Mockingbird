# Mockingbird's Python preload: `mockingbird proxy env` puts this file's
# directory first on PYTHONPATH, so Python imports it at startup as
# `sitecustomize`. It covers the clients that ignore HTTPS_PROXY and
# SSL_CERT_FILE on their own:
#   - certifi.where() names the bundle in SSL_CERT_FILE (system roots plus
#     the local CA), so code that pins certifi's roots trusts the proxy;
#   - a plain urllib3.PoolManager (OpenAPI-generated SDKs such as
#     plaid-python, boto-style clients) sends requests for hosts outside
#     NO_PROXY through HTTPS_PROXY, as requests and httpx already do.
# Subclasses of PoolManager (ProxyManager, requests' pools) are left alone.
# It then runs the next sitecustomize on sys.path, if there is one.
import os
import sys


def _mockingbird_install():
    proxy = os.environ.get("HTTPS_PROXY") or os.environ.get("https_proxy")
    bundle = os.environ.get("SSL_CERT_FILE")
    if not proxy or not os.environ.get("MOCKINGBIRD_CA_FILE"):
        return
    if bundle and os.path.isfile(bundle):
        try:
            import certifi
            import certifi.core
        except ImportError:
            pass
        else:
            certifi.where = certifi.core.where = lambda: bundle
    try:
        from urllib3 import poolmanager
        from urllib3.util import parse_url
    except ImportError:
        return
    from urllib.request import proxy_bypass_environment

    base = poolmanager.PoolManager
    if getattr(base.urlopen, "_mockingbird", False):
        return
    plain_urlopen = base.urlopen

    def urlopen(self, method, url, redirect=True, **kw):
        if type(self) is base:
            target = parse_url(url)
            if target.scheme in ("http", "https") and target.host and not proxy_bypass_environment(
                target.host
            ):
                via = self.__dict__.get("_mockingbird_proxy")
                if via is None:
                    via = poolmanager.ProxyManager(
                        proxy, headers=self.headers, **self.connection_pool_kw
                    )
                    self.__dict__["_mockingbird_proxy"] = via
                return via.urlopen(method, url, redirect=redirect, **kw)
        return plain_urlopen(self, method, url, redirect=redirect, **kw)

    urlopen._mockingbird = True
    base.urlopen = urlopen


def _mockingbird_chain():
    here = os.path.dirname(os.path.abspath(__file__))
    for entry in sys.path:
        if os.path.abspath(entry or ".") == here:
            continue
        candidate = os.path.join(entry or ".", "sitecustomize.py")
        if os.path.isfile(candidate):
            with open(candidate, encoding="utf-8") as f:
                code = compile(f.read(), candidate, "exec")
            exec(code, {"__name__": "sitecustomize", "__file__": candidate})
            return


try:
    _mockingbird_install()
except Exception as error:  # never break the app's startup
    sys.stderr.write("mockingbird: Python proxy preload skipped: %s\n" % error)
_mockingbird_chain()
