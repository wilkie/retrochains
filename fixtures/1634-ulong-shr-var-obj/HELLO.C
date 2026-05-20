int main(void) {
  unsigned long a = 0x10000UL;
  int n = 4;
  unsigned long r = a >> n;
  return (int)r;
}
