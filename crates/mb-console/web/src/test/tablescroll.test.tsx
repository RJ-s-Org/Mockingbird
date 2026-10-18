import { act, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { columnHelper, DataTable } from "../components/DataTable";

type Row = { name: string };
const col = columnHelper<Row>();
const columns = [col.accessor("name", { header: "Name" })];

/** Pretends every element is `client` wide with `scroll` px of content, scrolled to `left`. */
function layout(scroll: number, client: number, left = 0) {
  vi.spyOn(HTMLElement.prototype, "scrollWidth", "get").mockReturnValue(scroll);
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(client);
  vi.spyOn(HTMLElement.prototype, "scrollLeft", "get").mockReturnValue(left);
}

describe("table scroll affordance", () => {
  afterEach(() => vi.restoreAllMocks());

  it("a table wider than its container says it scrolls and shows an edge fade", () => {
    layout(800, 390);
    render(<DataTable caption="Things" columns={columns} data={[{ name: "a" }]} />);
    const scroller = screen.getByTestId("table-scroll");
    expect(scroller.getAttribute("data-overflow")).toBe("right");
    expect(screen.getByText(/Scroll sideways for more/)).toBeTruthy();
    // Focusable, so keyboard users can scroll it.
    expect(scroller.getAttribute("tabindex")).toBe("0");
  });

  it("scrolled to the end, only the left edge fades and the hint goes", () => {
    layout(800, 390, 410);
    render(<DataTable caption="Things" columns={columns} data={[{ name: "a" }]} />);
    const scroller = screen.getByTestId("table-scroll");
    act(() => {
      scroller.dispatchEvent(new Event("scroll"));
    });
    expect(scroller.getAttribute("data-overflow")).toBe("left");
    expect(screen.queryByText(/Scroll sideways for more/)).toBeNull();
  });

  it("a table that fits shows no affordance", () => {
    layout(390, 390);
    render(<DataTable caption="Things" columns={columns} data={[{ name: "a" }]} />);
    const scroller = screen.getByTestId("table-scroll");
    expect(scroller.getAttribute("data-overflow")).toBe("none");
    expect(scroller.getAttribute("tabindex")).toBeNull();
    expect(screen.queryByText(/Scroll sideways for more/)).toBeNull();
  });
});
