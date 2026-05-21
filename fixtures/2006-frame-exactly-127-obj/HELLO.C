int main(void) {
  char a[127];
  a[0] = 'A';
  a[126] = 'Z';
  return a[0] + a[126];
}
