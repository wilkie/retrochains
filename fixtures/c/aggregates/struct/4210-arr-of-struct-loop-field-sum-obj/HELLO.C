struct Pt { int x; int y; };
struct Pt pts[3] = {{1, 2}, {3, 4}, {5, 6}};
int main()
{
  int sum;
  int i;

  sum = 0;
  for (i = 0; i < 3; i++)
    sum += pts[i].y;
  return sum;
}
