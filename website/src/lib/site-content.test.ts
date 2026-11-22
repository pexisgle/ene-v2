import { describe, expect, it } from "vite-plus/test";
import { FLOW_STEPS, GUIDE_STEPS, NAV_ITEMS, SITE_META, TASK_EXAMPLES } from "./site-content.ts";

describe("site-content", () => {
  it("defines matching section anchors for navigation items", () => {
    expect(NAV_ITEMS.map((item) => item.href)).toEqual(["#examples", "#guide", "#inside"]);
  });

  it("includes all five capability examples and four ordered flow steps", () => {
    expect(TASK_EXAMPLES).toHaveLength(5);
    expect(GUIDE_STEPS.map((step) => step.number)).toEqual([1, 2]);
    expect(FLOW_STEPS.map((step) => step.id)).toEqual(["talk", "delegate", "work", "reply"]);
    expect(SITE_META.title).toContain("ene");
  });
});
