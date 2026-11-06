window.__ModuleLoader__.load({ id: "arca-canvas-dsh", factory: (require) => {
const module = { exports: {} }; const exports = module.exports;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __export = (target, all) => {
  for (var name2 in all)
    __defProp(target, name2, { get: all[name2], enumerable: true });
};
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key) && key !== except)
        __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
  }
  return to;
};
var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

// src/client.js
var client_exports = {};
__export(client_exports, {
  CanvasTab: () => CanvasTab,
  apply: () => apply,
  inject: () => inject,
  name: () => name
});
module.exports = __toCommonJS(client_exports);
var import_react = require("react");
var name = "arca-canvas";
var inject = ["betterSidebar"];
function CanvasTab() {
  return (0, import_react.createElement)("iframe", {
    src: "/arca-canvas",
    title: "Arca canvas",
    style: { display: "block", width: "100%", height: "100%", border: 0 }
  });
}
function apply(ctx) {
  if (!ctx.betterSidebar) return;
  ctx.effect(() => ctx.betterSidebar.registerTab({
    id: "arca-canvas",
    title: "Arca canvas",
    single: true,
    component: CanvasTab
  }));
}
return module.exports;
} });
