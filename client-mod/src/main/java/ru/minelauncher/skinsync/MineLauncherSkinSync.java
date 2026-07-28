package ru.minelauncher.skinsync;

import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.entity.player.PlayerEvent;
import net.neoforged.neoforge.event.server.ServerStoppedEvent;
import ru.minelauncher.skinsync.network.SkinNetwork;
import ru.minelauncher.skinsync.server.ServerSkinStore;

@Mod(MineLauncherSkinSync.MOD_ID)
public final class MineLauncherSkinSync {
    public static final String MOD_ID = "minelauncher_skin_sync";

    public MineLauncherSkinSync(IEventBus modBus) {
        modBus.addListener(SkinNetwork::register);
        NeoForge.EVENT_BUS.addListener(this::onPlayerLoggedOut);
        NeoForge.EVENT_BUS.addListener(this::onServerStopped);
    }

    private void onPlayerLoggedOut(PlayerEvent.PlayerLoggedOutEvent event) {
        ServerSkinStore.remove(event.getEntity().getUUID());
    }

    private void onServerStopped(ServerStoppedEvent event) {
        ServerSkinStore.clear();
    }
}
