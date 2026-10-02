import { describe, expect, it } from "vitest";
import {
  formatPhoneInput,
  formatPhoneNumber,
  isValidPhoneNumber,
  normalizePhoneNumber,
} from "./phone";

describe("normalizePhoneNumber", () => {
  it("canonicalizes equivalent valid US numbers", () => {
    expect(normalizePhoneNumber("2025550123")).toBe("+12025550123");
    expect(normalizePhoneNumber("1 (202) 555-0123")).toBe("+12025550123");
    expect(normalizePhoneNumber("+1 202.555.0123")).toBe("+12025550123");
  });

  it("accepts explicit international numbers", () => {
    expect(normalizePhoneNumber("+44 20 7946 0018")).toBe("+442079460018");
  });

  it("rejects incomplete, malformed, and extension-bearing values", () => {
    for (const value of [
      "",
      "202555",
      "20255501234",
      "555",
      "alice",
      "Call 2025550123",
      "2025550123 ext 4",
      "+1+2025550123",
    ]) {
      expect(normalizePhoneNumber(value)).toBeNull();
      expect(isValidPhoneNumber(value)).toBe(false);
    }
  });
});

describe("formatPhoneNumber", () => {
  it("uses US national format and explicit international format", () => {
    expect(formatPhoneNumber("+12025550123")).toBe("(202) 555-0123");
    expect(formatPhoneNumber("2025550123")).toBe("(202) 555-0123");
    expect(formatPhoneNumber("+442079460018")).toBe("+44 20 7946 0018");
  });

  it("leaves invalid values and names unchanged", () => {
    expect(formatPhoneNumber("Aurora")).toBe("Aurora");
    expect(formatPhoneNumber("555")).toBe("555");
    expect(formatPhoneNumber("2025550123 ext 4")).toBe("2025550123 ext 4");
  });
});

describe("formatPhoneInput", () => {
  it("formats a US number as it is typed", () => {
    expect(formatPhoneInput("2")).toBe("2");
    expect(formatPhoneInput("202")).toBe("(202)");
    expect(formatPhoneInput("2025")).toBe("(202) 5");
    expect(formatPhoneInput("2025550123")).toBe("(202) 555-0123");
  });

  it("preserves invalid or multi-recipient input for correction", () => {
    expect(formatPhoneInput("202abc")).toBe("202abc");
    expect(formatPhoneInput("2025550123, 3035550123")).toBe(
      "2025550123, 3035550123",
    );
    expect(formatPhoneInput("2025550123 ext 4")).toBe("2025550123 ext 4");
  });

  it("preserves a whitespace boundary after a complete number", () => {
    expect(formatPhoneInput("2025550100 2025550101")).toBe(
      "2025550100 2025550101",
    );
    expect(formatPhoneInput("(202) 555-0100 2025550101")).toBe(
      "(202) 555-0100 2025550101",
    );
  });

  it("preserves overlong input instead of truncating it", () => {
    expect(formatPhoneInput("2025550100123456")).toBe("2025550100123456");
  });

  it("continues formatting one spaced valid number", () => {
    expect(formatPhoneInput("202 555 0100")).toBe("(202) 555-0100");
    expect(formatPhoneInput("+44 20 7946 0018")).toBe("+44 20 7946 0018");
  });
});
