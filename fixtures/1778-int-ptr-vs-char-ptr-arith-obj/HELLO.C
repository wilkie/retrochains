int main(void) {
  char ca[4];
  int ia[4];
  char *cp = ca;
  int *ip = ia;
  cp++;
  ip++;
  ca[0] = 5;
  ia[0] = 10;
  *cp = 7;
  *ip = 20;
  return ca[1] + (int)ia[1];
}
