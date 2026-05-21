int swap_halves(int x) {
  asm mov ax, x
  asm xchg ah, al
  return _AX;
}
int main(void) {
  return swap_halves(0x1234);
}
