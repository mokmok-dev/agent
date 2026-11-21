import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import { commandOf, isCancelText, THREAD_COMMANDS } from "./commands.ts";

describe("commandOf", () => {
  test("recognises each command in any case and padding", () => {
    for (const command of THREAD_COMMANDS) {
      expect(commandOf(command)).toBe(command);
      expect(commandOf(command.toUpperCase())).toBe(command);
      expect(commandOf(`  ${command}  `)).toBe(command);
      expect(commandOf(`\t${command}\n`)).toBe(command);
      expect(commandOf(` ${command.toUpperCase()} `)).toBe(command);
    }
  });

  test("a command word inside a sentence is a prompt", () => {
    expect(commandOf("stop now")).toBeUndefined();
    expect(commandOf("please cancel")).toBeUndefined();
    expect(commandOf("stop; then restart")).toBeUndefined();
    expect(commandOf("cancelled")).toBeUndefined();
    expect(commandOf("stops")).toBeUndefined();
  });

  test("empty text is not a command", () => {
    expect(commandOf("")).toBeUndefined();
    expect(commandOf("   ")).toBeUndefined();
  });

  test("isCancelText agrees with commandOf over any text", () => {
    fc.assert(
      fc.property(fc.string(), (text) => {
        expect(isCancelText(text)).toBe(commandOf(text) !== undefined);
      }),
    );
  });

  test("a command found by commandOf is always the trimmed lower-case text", () => {
    fc.assert(
      fc.property(fc.string(), (text) => {
        const command = commandOf(text);
        if (command === undefined) return;
        expect(text.trim().toLowerCase()).toBe(command);
      }),
    );
  });
});
