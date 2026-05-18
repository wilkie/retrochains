unsigned long g;
int main() {
  unsigned int x;
  g = 0xFFFFFFFF;
  x = 0x0FF0;
  g &= x;
  return 0;
}
