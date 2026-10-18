import { createFileRoute } from "@tanstack/react-router";
import AddMachines from "@/views/add-machines";

export const Route = createFileRoute("/_app/runners/add")({ component: AddMachines });
