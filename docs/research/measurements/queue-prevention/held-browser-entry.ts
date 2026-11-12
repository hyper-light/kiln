import { readHeld, useHeld } from "../../../../ui/src/api/held";
import { setMailScope } from "../../../../ui/src/api/client";
setMailScope("browser-account-a");
const ready = useHeld.getState().ready();
Object.assign(window, { heldFixture: { readHeld, useHeld, setMailScope, ready } });
