// Runtime shim injected into every proxied HTML page.
//
// The server-side HTMLRewriter only sees URLs that exist in the markup. Pages
// build plenty of URLs at runtime (fetch(), XHR, `img.src = ...`, innerHTML
// templates). This function is serialised with `.toString()` and inlined as
// `(<fn>)()` at the top of <head>, so it must be self-contained: no imports,
// no references to module scope, ES5-ish syntax so old WebViews run it.
//
// It reads `window.__CATPROXY__ = { base }` (set by index.js) and rewrites any
// http(s) URL the page produces into `<this origin>/<absolute upstream URL>`.
export function catproxyShim() {
  var cfg = window.__CATPROXY__;
  if (!cfg || !cfg.base || window.__CATPROXY_INSTALLED__) return;
  window.__CATPROXY_INSTALLED__ = true;

  var ORIGIN = location.origin;
  var PREFIX = ORIGIN + '/';
  var URL_ATTRS = ['src', 'href', 'srcset', 'poster', 'action', 'formaction', 'data-src'];
  var SKIP = /^\s*(data:|blob:|about:|javascript:|mailto:|tel:|#)/i;

  function proxify(u) {
    if (u == null) return u;
    u = String(u);
    if (!u || SKIP.test(u)) return u;
    if (u.indexOf(PREFIX + 'http') === 0) return u; // already proxied
    var abs;
    try {
      // A URL that already points at our origin but isn't in proxied form is a
      // root-relative path the browser resolved against us; re-resolve upstream.
      if (u.indexOf(PREFIX) === 0) u = u.slice(ORIGIN.length);
      abs = new URL(u, cfg.base).href;
    } catch (e) {
      return u;
    }
    if (!/^https?:/i.test(abs)) return u;
    return PREFIX + abs;
  }

  function proxifySrcset(v) {
    if (!v) return v;
    return String(v).split(',').map(function (part) {
      var m = part.trim().match(/^(\S+)(\s+.*)?$/);
      return m ? proxify(m[1]) + (m[2] || '') : part;
    }).join(', ');
  }

  function proxifyAttr(name, value) {
    return name === 'srcset' ? proxifySrcset(value) : proxify(value);
  }

  // Rewrite URL attributes inside an HTML string (innerHTML & friends). Done as
  // a string pass so the request never fires against the original host.
  var ATTR_RE = /(\s(?:src|href|srcset|poster|action|formaction|data-src)\s*=\s*)(["'])([\s\S]*?)\2/gi;
  function proxifyHtml(html) {
    if (typeof html !== 'string' || html.indexOf('=') < 0) return html;
    return html.replace(ATTR_RE, function (_, lead, q, val) {
      var name = lead.trim().split('=')[0].toLowerCase();
      return lead + q + proxifyAttr(name, val) + q;
    });
  }

  // --- fetch / XHR -----------------------------------------------------------
  var origFetch = window.fetch;
  if (origFetch) {
    window.fetch = function (input, init) {
      if (typeof input === 'string' || input instanceof URL) {
        input = proxify(String(input));
      } else if (input && typeof input.url === 'string') {
        input = new Request(proxify(input.url), input);
      }
      return origFetch.call(this, input, init);
    };
  }
  var origOpen = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function (method, url) {
    var args = Array.prototype.slice.call(arguments);
    args[1] = proxify(url);
    return origOpen.apply(this, args);
  };

  // --- attribute & property setters -----------------------------------------
  var origSetAttribute = Element.prototype.setAttribute;
  Element.prototype.setAttribute = function (name, value) {
    var n = String(name).toLowerCase();
    if (URL_ATTRS.indexOf(n) >= 0) value = proxifyAttr(n, value);
    return origSetAttribute.call(this, name, value);
  };

  function patchProp(proto, prop, fn) {
    if (!proto) return;
    var d = Object.getOwnPropertyDescriptor(proto, prop);
    if (!d || !d.set) return;
    Object.defineProperty(proto, prop, {
      configurable: true,
      enumerable: d.enumerable,
      get: d.get,
      set: function (v) { d.set.call(this, fn(v)); }
    });
  }
  [
    [window.HTMLImageElement, 'src'], [window.HTMLImageElement, 'srcset'],
    [window.HTMLScriptElement, 'src'], [window.HTMLLinkElement, 'href'],
    [window.HTMLAnchorElement, 'href'], [window.HTMLIFrameElement, 'src'],
    [window.HTMLSourceElement, 'src'], [window.HTMLSourceElement, 'srcset'],
    [window.HTMLMediaElement, 'src'], [window.HTMLVideoElement, 'poster'],
    [window.HTMLEmbedElement, 'src'], [window.HTMLObjectElement, 'data'],
    [window.HTMLFormElement, 'action']
  ].forEach(function (p) {
    if (p[0]) patchProp(p[0].prototype, p[1], p[1] === 'srcset' ? proxifySrcset : proxify);
  });
  patchProp(Element.prototype, 'innerHTML', proxifyHtml);
  patchProp(Element.prototype, 'outerHTML', proxifyHtml);
  var origInsertAdjacentHTML = Element.prototype.insertAdjacentHTML;
  if (origInsertAdjacentHTML) {
    Element.prototype.insertAdjacentHTML = function (pos, html) {
      return origInsertAdjacentHTML.call(this, pos, proxifyHtml(html));
    };
  }

  // --- safety net: anything that slipped through (DOMParser, templates, ...) --
  function fixElement(el) {
    if (!el || el.nodeType !== 1) return;
    for (var i = 0; i < URL_ATTRS.length; i++) {
      var a = URL_ATTRS[i];
      if (!el.hasAttribute(a)) continue;
      var v = el.getAttribute(a), nv = proxifyAttr(a, v);
      if (nv !== v) origSetAttribute.call(el, a, nv);
    }
  }
  function fixTree(root) {
    fixElement(root);
    if (root.querySelectorAll) {
      var all = root.querySelectorAll('[' + URL_ATTRS.join('],[') + ']');
      for (var i = 0; i < all.length; i++) fixElement(all[i]);
    }
  }
  if (window.MutationObserver) {
    var mo = new MutationObserver(function (records) {
      for (var i = 0; i < records.length; i++) {
        var r = records[i];
        if (r.type === 'attributes') fixElement(r.target);
        else for (var j = 0; j < r.addedNodes.length; j++) fixTree(r.addedNodes[j]);
      }
    });
    mo.observe(document.documentElement || document, {
      childList: true, subtree: true, attributes: true, attributeFilter: URL_ATTRS
    });
  }

  window.__CATPROXY__.proxify = proxify;
}
