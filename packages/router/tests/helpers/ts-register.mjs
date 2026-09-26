import { register } from "node:module";

register(new URL("./ts-hook.mjs", import.meta.url).href);
