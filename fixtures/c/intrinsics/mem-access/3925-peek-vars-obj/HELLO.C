int peek(unsigned, unsigned);
int main(void) {
  unsigned seg = 0x0040;
  unsigned off = 0x0017;
  return peek(seg, off);
}
