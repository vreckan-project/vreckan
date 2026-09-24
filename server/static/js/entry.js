// Module entry point (loaded by the <script> tag in index.html, with a
// ?v=<hash> cache-buster appended by the server).
//
// This is a SEPARATE module from app.js on purpose: the sub-modules under
// app/ import app.js by its plain URL, so if the <script> tag loaded app.js
// directly (with the ?v= hash) the module graph would contain TWO instances
// of app.js — the hashed entry and the plain one the sub-modules share — and
// each would run init(), double-binding every listener (two launch POSTs per
// click, two sessions per launch). Loading a thin entry that imports app.js
// once keeps app.js a single shared instance.
import { init } from "./app.js";

init();
