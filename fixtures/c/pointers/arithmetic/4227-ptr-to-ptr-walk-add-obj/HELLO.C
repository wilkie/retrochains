int main(void) {
  int x = 10;
  int y = 20;
  int z = 30;
  int *row[3];
  int **pp;
  int sum;
  row[0] = &x;
  row[1] = &y;
  row[2] = &z;
  pp = row;
  sum = **pp;
  sum = sum + **(pp + 2);
  return sum;
}
