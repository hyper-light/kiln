import { Link } from "react-router";
import { ArrowRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Optic } from "@/components/ui/misc";

export default function NotFound() {
  return (
    <div className="container-console page flex flex-col items-center py-24 text-center">
      <title>Not found · kiln</title>
      <Optic size={200} />
      <div className="eyebrow mt-8">
        <span className="spectrum-rule" /> 404 / Not found
      </div>
      <h1 className="mt-4 text-[40px] font-[450] tracking-[-0.05em]">Nothing at this path.</h1>
      <p className="mt-3 text-[14px] text-[#a1a4ad]">Try the command palette — Ctrl/⌘ K.</p>
      <Button asChild className="mt-8">
        <Link to="/">
          Back to overview <ArrowRight />
        </Link>
      </Button>
    </div>
  );
}
