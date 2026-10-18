import { createFileRoute } from "@tanstack/react-router";
import Deployments from "@/views/deployments/index";

export const Route = createFileRoute("/_app/deployments/")({ component: Deployments });
