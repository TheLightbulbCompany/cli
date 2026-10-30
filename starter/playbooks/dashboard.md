# Dashboard

Switches: database if the data arrives as a file.

**Builds first:** summarise the data before drawing anything: what each column is, totals, ranges,
gaps. Then stat cards, charts (`src/components/ui/chart.tsx`) and filters.

**Asks (one card at most):** which decision the dashboard serves, if the request doesn't say.

**Never asks:** chart types, colours, which metric goes first (lead with the one the request names).

**Done when:** one figure on the dashboard is checked by hand against the source, and the check is
written in `.lightbulb/plan.md`.
