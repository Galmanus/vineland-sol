import React from "react";
import { createRoot } from "react-dom/client";
import { PrivyProvider } from "@privy-io/react-auth";
import { createSolanaRpc, createSolanaRpcSubscriptions } from "@solana/kit";
import App from "./App.jsx";
import "./styles.css";

const appId = import.meta.env.VITE_PRIVY_APP_ID;

const root = createRoot(document.getElementById("root"));

if (appId) {
  root.render(
    <PrivyProvider
      appId={appId}
      config={{
        loginMethods: ["email"],
        appearance: { theme: "dark", accentColor: "#F5B83E", walletChainType: "solana-only" },
        embeddedWallets: { solana: { createOnLogin: "users-without-wallets" }, ethereum: { createOnLogin: "off" } },
        solana: {
          rpcs: {
            "solana:devnet": {
              rpc: createSolanaRpc("https://api.devnet.solana.com"),
              rpcSubscriptions: createSolanaRpcSubscriptions("wss://api.devnet.solana.com"),
            },
          },
        },
      }}
    >
      <App privy />
    </PrivyProvider>,
  );
} else {
  root.render(<App privy={false} />);
}
