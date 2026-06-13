int main(void) {
  int i;
  char c;
  i = 0x12AB;
  c = *((char *)&i);
  return c;
}
