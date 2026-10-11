#!/usr/bin/env node
/**
 * Make the exported site runnable on old WebViews (Android TV ships Chrome 66).
 *
 * Two problems are solved here, after `next build` and before the Android
 * assets are packaged:
 *  1. SYNTAX: bundles contain ES2020 syntax (`?.`, `??`) that Chrome 66 cannot
 *     parse -> white screen. Each JS file is lowered with esbuild to a
 *     chrome66-compatible target.
 *  2. RUNTIME APIs: `globalThis` (71), `queueMicrotask` (71),
 *     `Object.fromEntries` (73), `Promise.allSettled` (76), `Object.hasOwn`
 *     (93), `structuredClone` (98) are referenced by the bundles but missing
 *     on old WebViews. A tiny polyfill prelude is injected as the first inline
 *     <script> of every exported HTML document so it runs before any chunk.
 *
 * The pass is idempotent (marker comment) and safe for modern browsers: the
 * polyfills are feature-detected and the lowered syntax is semantically
 * identical.
 */
import { readdir, readFile, writeFile, stat } from "node:fs/promises";
import { join, extname } from "node:path";
import { createRequire } from "node:module";

// esbuild lives in the repo root's node_modules (hoisted); resolve from there
// so this script works no matter where node_modules ended up.
const require = createRequire(import.meta.url);
let transform;
try {
  ({ transform } = require("esbuild"));
} catch {
  ({ transform } = require(join(process.cwd(), "..", "node_modules", "esbuild")));
}

const OUT_DIR = join(process.cwd(), "out");
const MARKER = "/*dtv-legacy-compat*/";
const HTML_MARKER = "dtv-legacy-polyfills";

const PRELUDE = `(function(){
try{
if(typeof globalThis==="undefined"){try{Object.defineProperty(Object.prototype,"__dtvG",{get:function(){return this},configurable:true});__dtvG.globalThis=__dtvG;delete Object.prototype.__dtvG;}catch(e){window.globalThis=window;}}
if(!window.queueMicrotask){window.queueMicrotask=function(f){Promise.resolve().then(f)["catch"](function(e){setTimeout(function(){throw e;});});};}
if(!Object.fromEntries){Object.fromEntries=function(it){var o={};var add=function(e){o[e[0]]=e[1];};Array.from(it,add);return o;};}
if(!Promise.allSettled){Promise.allSettled=function(ps){return Promise.all(Array.from(ps,function(p){return Promise.resolve(p).then(function(v){return{status:"fulfilled",value:v};},function(r){return{status:"rejected",reason:r};});}));};}
if(!Object.hasOwn){Object.hasOwn=function(o,k){return Object.prototype.hasOwnProperty.call(Object(o),k);};}
if(!window.structuredClone){window.structuredClone=function(v){return JSON.parse(JSON.stringify(v));};}
if(!Array.prototype.flat){Object.defineProperty(Array.prototype,"flat",{configurable:true,writable:true,value:function(d){d=d===undefined?1:d;var out=[];for(var i=0;i<this.length;i++){var e=this[i];if(Array.isArray(e)&&d>0){out.push.apply(out,e.flat(d-1));}else{out.push(e);}}return out;}});}
if(!Array.prototype.at){Object.defineProperty(Array.prototype,"at",{configurable:true,writable:true,value:function(n){n=Math.trunc(n)||0;if(n<0){n+=this.length;}if(n<0||n>=this.length){return undefined;}return this[n];}});}
if(!String.prototype.replaceAll){Object.defineProperty(String.prototype,"replaceAll",{configurable:true,writable:true,value:function(a,b){if(a instanceof RegExp){return this.replace(a,b);}return this.split(a).join(b);}});}
}catch(e){if(window.console){console.warn("legacy polyfills failed",e);}}
})();`;

async function* walk(dir) {
  for (const name of await readdir(dir)) {
    const p = join(dir, name);
    const st = await stat(p);
    if (st.isDirectory()) yield* walk(p);
    else yield p;
  }
}

let jsCount = 0;
let jsSkipped = 0;
let htmlCount = 0;

for await (const file of walk(OUT_DIR)) {
  const ext = extname(file).toLowerCase();
  if (ext === ".js" || ext === ".mjs") {
    const src = await readFile(file, "utf8");
    if (src.includes(MARKER)) {
      jsSkipped++;
      continue;
    }
    try {
      const res = await transform(src, {
        target: ["chrome66"],
        minify: true,
        // target=chrome66 already forces esbuild to lower every feature Chrome
        // 66 lacks (`?.`, `??`, class fields, static blocks, ...). It keeps the
        // module format it detects (Chrome 66 supports ESM + dynamic import).
      });
      await writeFile(file, `${MARKER}\n${res.code}`, "utf8");
      jsCount++;
    } catch (e) {
      // Never break the build over the legacy pass: modern devices don't need
      // it, and a skipped file still works everywhere else.
      console.warn(`[legacy-compat] transform SKIPPED for ${file}: ${e.message}`);
      jsSkipped++;
    }
  } else if (ext === ".html") {
    const src = await readFile(file, "utf8");
    if (src.includes(HTML_MARKER)) continue;
    if (!/<head/i.test(src)) continue;
    const inject = `<script id="${HTML_MARKER}">${PRELUDE}${MARKER}</script>`;
    const out = src.replace(/<head([^>]*)>/i, (m) => `${m}${inject}`);
    await writeFile(file, out, "utf8");
    htmlCount++;
  }
}

console.log(
  `[legacy-compat] lowered ${jsCount} JS file(s) to chrome66 (${jsSkipped} skipped), injected polyfills into ${htmlCount} HTML doc(s)`
);
