package ru.minelauncher.skinsync.client.mixin;

import net.minecraft.client.player.AbstractClientPlayer;
import net.minecraft.client.resources.PlayerSkin;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;
import ru.minelauncher.skinsync.client.ClientSkinManager;

@Mixin(AbstractClientPlayer.class)
public abstract class AbstractClientPlayerMixin {
    @Inject(method = "getSkin", at = @At("HEAD"), cancellable = true)
    private void minelauncherSkinSync$getSkin(
            CallbackInfoReturnable<PlayerSkin> callback
    ) {
        AbstractClientPlayer player = (AbstractClientPlayer) (Object) this;
        PlayerSkin skin = ClientSkinManager.get(player.getUUID());
        if (skin != null) {
            callback.setReturnValue(skin);
        }
    }
}
