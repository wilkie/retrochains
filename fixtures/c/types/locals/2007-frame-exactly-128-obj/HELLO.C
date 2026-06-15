int main(void) {
  char a[128];
  a[0] = 'A';
  a[127] = 'Z';
  return a[0] + a[127];
}
