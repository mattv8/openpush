import { AsYouType, parsePhoneNumberFromString } from "libphonenumber-js/min";

const phoneCharacters = /^\+?[\d\s().-]+$/;

function hasAllowedPhoneCharacters(value: string): boolean {
  return phoneCharacters.test(value);
}

function hasWhitespaceNumberBoundary(value: string): boolean {
  if (normalizePhoneNumber(value)) return false;

  for (const match of value.matchAll(/\s+/g)) {
    const prefix = value.slice(0, match.index).trimEnd();
    const remainder = value.slice((match.index ?? 0) + match[0].length);
    if (normalizePhoneNumber(prefix) && /\d/.test(remainder)) return true;
  }

  return false;
}

/**
 * Returns a canonical E.164 number when the input has a valid numbering-plan
 * structure. This is local validation only; it does not establish reachability
 * or SMS capability.
 */
export function normalizePhoneNumber(input: string): string | null {
  const value = input.trim();
  if (!value || !hasAllowedPhoneCharacters(value)) return null;

  const phoneNumber = parsePhoneNumberFromString(value, {
    defaultCountry: "US",
    extract: false,
  });

  return phoneNumber?.isValid() ? phoneNumber.number : null;
}

export function isValidPhoneNumber(input: string): boolean {
  return normalizePhoneNumber(input) !== null;
}

export function formatPhoneNumber(input: string): string {
  const normalized = normalizePhoneNumber(input);
  if (!normalized) return input;

  const phoneNumber = parsePhoneNumberFromString(normalized);
  if (!phoneNumber) return input;

  return phoneNumber.countryCallingCode === "1"
    ? phoneNumber.formatNational()
    : phoneNumber.formatInternational();
}

export function formatPhoneInput(input: string): string {
  if (!input || !hasAllowedPhoneCharacters(input)) return input;
  if (
    input.replace(/\D/g, "").length > 15 ||
    hasWhitespaceNumberBoundary(input)
  ) {
    return input;
  }

  const formatted = new AsYouType("US").input(input);
  return formatted || input;
}
